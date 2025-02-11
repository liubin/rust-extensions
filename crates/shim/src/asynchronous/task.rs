/*
   Copyright The containerd Authors.

   Licensed under the Apache License, Version 2.0 (the "License");
   you may not use this file except in compliance with the License.
   You may obtain a copy of the License at

       http://www.apache.org/licenses/LICENSE-2.0

   Unless required by applicable law or agreed to in writing, software
   distributed under the License is distributed on an "AS IS" BASIS,
   WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
   See the License for the specific language governing permissions and
   limitations under the License.
*/

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use log::{debug, info};
use tokio::sync::{MappedMutexGuard, Mutex, MutexGuard};

use containerd_shim_protos::api::DeleteResponse;
use containerd_shim_protos::protobuf::well_known_types::Timestamp;
use containerd_shim_protos::protobuf::SingularPtrField;
use containerd_shim_protos::shim_async::Task;
use containerd_shim_protos::ttrpc;
use containerd_shim_protos::ttrpc::r#async::TtrpcContext;

use crate::api::{
    CreateTaskRequest, CreateTaskResponse, DeleteRequest, Empty, ExecProcessRequest, KillRequest,
    ResizePtyRequest, ShutdownRequest, StartRequest, StartResponse, StateRequest, StateResponse,
    Status, WaitRequest, WaitResponse,
};
use crate::asynchronous::container::{Container, ContainerFactory};
use crate::asynchronous::ExitSignal;
use crate::util::AsOption;
use crate::TtrpcResult;

/// TaskService is a Task template struct, it is considered a helper struct,
/// which has already implemented `Task` trait, so that users can make it the type `T`
/// parameter of `Service`, and implements their own `ContainerFactory` and `Container`.
pub struct TaskService<F, C> {
    pub factory: F,
    pub containers: Arc<Mutex<HashMap<String, C>>>,
    pub namespace: String,
    pub exit: Arc<ExitSignal>,
}

impl<F, C> TaskService<F, C>
where
    F: Default,
{
    pub fn new(ns: &str, exit: Arc<ExitSignal>) -> Self {
        Self {
            factory: Default::default(),
            containers: Arc::new(Mutex::new(Default::default())),
            namespace: ns.to_string(),
            exit,
        }
    }
}

impl<F, C> TaskService<F, C> {
    pub async fn get_container(&self, id: &str) -> TtrpcResult<MappedMutexGuard<'_, C>> {
        let mut containers = self.containers.lock().await;
        containers.get_mut(id).ok_or_else(|| {
            ttrpc::Error::RpcStatus(ttrpc::get_status(
                ttrpc::Code::NOT_FOUND,
                format!("can not find container by id {}", id),
            ))
        })?;
        let container = MutexGuard::map(containers, |m| m.get_mut(id).unwrap());
        Ok(container)
    }
}

#[async_trait]
impl<F, C> Task for TaskService<F, C>
where
    F: ContainerFactory<C> + Sync + Send,
    C: Container + Sync + Send + 'static,
{
    async fn state(&self, _ctx: &TtrpcContext, req: StateRequest) -> TtrpcResult<StateResponse> {
        let container = self.get_container(req.get_id()).await?;
        let exec_id = req.get_exec_id().as_option();
        let resp = container.state(exec_id).await?;
        Ok(resp)
    }

    async fn create(
        &self,
        _ctx: &TtrpcContext,
        req: CreateTaskRequest,
    ) -> TtrpcResult<CreateTaskResponse> {
        info!("Create request for {:?}", &req);
        // Note: Get containers here is for getting the lock,
        // to make sure no other threads manipulate the containers metadata;
        let mut containers = self.containers.lock().await;

        let ns = self.namespace.as_str();
        let id = req.id.as_str();

        let container = self.factory.create(ns, &req).await?;
        let mut resp = CreateTaskResponse::new();
        resp.pid = container.pid().await as u32;

        containers.insert(id.to_string(), container);
        info!("Create request for {} returns pid {}", id, resp.pid);
        Ok(resp)
    }

    async fn start(&self, _ctx: &TtrpcContext, req: StartRequest) -> TtrpcResult<StartResponse> {
        info!("Start request for {:?}", &req);
        let mut container = self.get_container(req.get_id()).await?;
        let pid = container.start(req.exec_id.as_str().as_option()).await?;

        let mut resp = StartResponse::new();
        resp.pid = pid as u32;
        info!("Start request for {:?} returns pid {}", req, resp.get_pid());
        Ok(resp)
    }

    async fn delete(&self, _ctx: &TtrpcContext, req: DeleteRequest) -> TtrpcResult<DeleteResponse> {
        info!("Delete request for {:?}", &req);
        let mut containers = self.containers.lock().await;
        let container = containers.get_mut(req.get_id()).ok_or_else(|| {
            ttrpc::Error::RpcStatus(ttrpc::get_status(
                ttrpc::Code::NOT_FOUND,
                format!("can not find container by id {}", req.get_id()),
            ))
        })?;
        let exec_id_opt = req.get_exec_id().as_option();
        let (pid, exit_status, exited_at) = container.delete(exec_id_opt).await?;
        self.factory.cleanup(&*self.namespace, container).await?;
        if req.get_exec_id().is_empty() {
            containers.remove(req.get_id());
        }
        let mut resp = DeleteResponse::new();
        resp.set_exited_at(exited_at);
        resp.set_pid(pid as u32);
        resp.set_exit_status(exit_status);
        info!(
            "Delete request for {} {} returns {:?}",
            req.get_id(),
            req.get_exec_id(),
            resp
        );
        Ok(resp)
    }

    async fn kill(&self, _ctx: &TtrpcContext, req: KillRequest) -> TtrpcResult<Empty> {
        info!("Kill request for {:?}", req);
        let mut container = self.get_container(req.get_id()).await?;
        container
            .kill(req.get_exec_id().as_option(), req.signal, req.all)
            .await?;
        info!("Kill request for {:?} returns successfully", req);
        Ok(Empty::new())
    }

    async fn exec(&self, _ctx: &TtrpcContext, req: ExecProcessRequest) -> TtrpcResult<Empty> {
        info!("Exec request for {:?}", req);
        let mut container = self.get_container(req.get_id()).await?;
        container.exec(req).await?;
        Ok(Empty::new())
    }

    async fn resize_pty(&self, _ctx: &TtrpcContext, req: ResizePtyRequest) -> TtrpcResult<Empty> {
        debug!(
            "Resize pty request for container {}, exec_id: {}",
            &req.id, &req.exec_id
        );
        let mut container = self.get_container(req.get_id()).await?;
        container
            .resize_pty(req.get_exec_id().as_option(), req.height, req.width)
            .await?;
        Ok(Empty::new())
    }

    async fn wait(&self, _ctx: &TtrpcContext, req: WaitRequest) -> TtrpcResult<WaitResponse> {
        info!("Wait request for {:?}", req);
        let exec_id = req.exec_id.as_str().as_option();
        let wait_rx = {
            let mut container = self.get_container(req.get_id()).await?;
            let state = container.state(exec_id).await?;
            if state.status != Status::RUNNING && state.status != Status::CREATED {
                let mut resp = WaitResponse::new();
                resp.exit_status = state.exit_status;
                resp.exited_at = state.exited_at;
                info!("Wait request for {:?} returns {:?}", req, &resp);
                return Ok(resp);
            }
            container
                .wait_channel(req.get_exec_id().as_option())
                .await?
        };

        wait_rx.await.unwrap_or_default();
        // get lock again.
        let container = self.get_container(req.get_id()).await?;
        let (_, code, exited_at) = container.get_exit_info(exec_id).await?;
        let mut resp = WaitResponse::new();
        resp.exit_status = code;
        let mut ts = Timestamp::new();
        if let Some(ea) = exited_at {
            ts.seconds = ea.unix_timestamp();
            ts.nanos = ea.nanosecond() as i32;
        }
        resp.exited_at = SingularPtrField::some(ts);
        info!("Wait request for {:?} returns {:?}", req, &resp);
        Ok(resp)
    }

    async fn shutdown(&self, _ctx: &TtrpcContext, _req: ShutdownRequest) -> TtrpcResult<Empty> {
        debug!("Shutdown request");
        let containers = self.containers.lock().await;
        if containers.len() > 0 {
            return Ok(Empty::new());
        }
        self.exit.signal();
        Ok(Empty::default())
    }
}
