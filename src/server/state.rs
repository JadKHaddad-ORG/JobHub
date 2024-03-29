use crate::server::ws::{IoType, ServerMessage, TaskIoChunk};

use super::{
    connection_manager::ConnectionManager,
    task::{Handle, Status, Task},
    ws::ClientMessage,
};
use axum::extract::ws::WebSocket;
use std::{
    collections::HashMap,
    net::SocketAddr,
    ops::Deref,
    path::PathBuf,
    sync::{
        atomic::{AtomicU32, Ordering},
        Arc,
    },
};
use tokio::{io::AsyncReadExt, sync::RwLock};

/// I want my [`ApiState`] to be [`Clone`] and [`Send`] and [`Sync`] as is.
/// So I'm wrapping [`ApiState::inner`] in an [`Arc`].
#[derive(Clone)]
pub struct ApiState {
    inner: Arc<ApiStateInner>,
}

impl ApiState {
    pub fn new(api_token: String, projects_dir: String) -> Self {
        Self {
            inner: Arc::new(ApiStateInner::new(api_token, projects_dir)),
        }
    }

    pub fn api_token_valid(&self, api_token: &str) -> bool {
        api_token == self.api_token
    }

    pub async fn accept_connection(self, socket: WebSocket, user_agent: String, addr: SocketAddr) {
        let (tx, mut rx) = tokio::sync::mpsc::channel::<ClientMessage>(100);

        self.inner
            .connection_manager
            .accept_connection(tx, socket, user_agent, addr)
            .await;

        tokio::spawn(async move {
            while let Some(msg) = rx.recv().await {
                // Deal with the message
                tracing::info!(?msg, "Received message from client");
            }
        });
    }
}

/// Collecting relevant data for a task.
struct TaskData {
    chat_id: String,
    handle: Handle,
}

pub struct ApiStateInner {
    api_token: String,
    connection_manager: Arc<ConnectionManager>,
    /// Contains all the tasks that are currently running.
    /// The key is the task id.
    tasks: Arc<RwLock<HashMap<String, TaskData>>>,
    /// I'm not wrapping [`ApiStateInner`] in a lock.
    /// So it's a good old [`AtomicU32`].
    current_id: AtomicU32,
    projects_dir: String,
}

impl ApiStateInner {
    pub fn new(api_token: String, projects_dir: String) -> Self {
        Self {
            api_token,
            connection_manager: Arc::new(ConnectionManager::new()),
            tasks: Arc::new(RwLock::new(HashMap::new())),
            current_id: AtomicU32::new(0),
            projects_dir,
        }
    }

    pub fn generate_random_chat_id(&self) -> String {
        uuid::Uuid::new_v4().to_string()
    }

    fn increment_current_task_id(&self) -> u32 {
        let id = self.current_id.load(Ordering::Relaxed);

        self.current_id.store(id + 1, Ordering::Relaxed);

        id
    }

    pub async fn run_download_task(
        &self,
        chat_id: String,
        download_url: url::Url,
        project_name: String,
    ) -> Result<String, std::io::Error> {
        // Let's create a directory for the project
        let project_dir = PathBuf::from(&self.projects_dir).join(project_name);
        tokio::fs::create_dir_all(&project_dir).await?;

        let id = self.increment_current_task_id().to_string();
        let task_id = id.clone();

        let timeout = std::time::Duration::from_secs(600);

        let (task, task_handle) = Task::new(id.clone());
        let task_data = TaskData {
            chat_id,
            handle: task_handle,
        };

        let mut tasks = self.tasks.write().await;
        tasks.insert(id.clone(), task_data);

        let tasks = self.tasks.clone();

        tokio::spawn(async move {
            task.run_download_and_unzip_from_download_url(timeout, download_url, project_dir)
                .await;

            // Keeping task in memory for 15 minutes after it's done.
            // simulating an in-memory database.

            tracing::debug!(id=%task_id, "Task finished. Waiting 15 minutes before removing it from memory");
            tokio::time::sleep(std::time::Duration::from_secs(900)).await;
            tracing::debug!(id=%task_id, "Removing task from memory");
            let mut tasks = tasks.write().await;
            tasks.remove(&task_id);
        });

        Ok(id)
    }

    pub async fn run_task(&self, chat_id: String) -> String {
        let id = self.increment_current_task_id().to_string();
        let task_id = id.clone();

        let timeout = std::time::Duration::from_secs(600);

        let (task, task_handle) = Task::new(id.clone());

        // TODO: Move to tests
        // {
        // Test canceling the task before running it. and expect it to be canceled immediately after running.
        // task_handle.cancel().await;

        // Test dropping the handle before running the task. and expect it to be canceled immediately after running.
        // drop(task_handle);
        // }

        let task_data = TaskData {
            chat_id,
            handle: task_handle,
        };

        let mut tasks = self.tasks.write().await;
        tasks.insert(id.clone(), task_data);

        let connection_manager = self.connection_manager.clone();
        let tasks = self.tasks.clone();
        tokio::spawn(async move {
            let (stdout_tx, mut stdout_rx) = tokio::io::duplex(100);
            let (stderr_tx, mut stderr_rx) = tokio::io::duplex(100);

            let stdout_task_id = task_id.clone();
            let stderr_task_id = task_id.clone();

            let stdout_connection_manager = connection_manager.clone();
            let stderr_connection_manager = connection_manager;

            // While forwarding the outputs we can save the chunks to the database or send them to a client.
            tokio::spawn(async move {
                let mut chunk = [0; 256];
                while let Ok(n) = stdout_rx.read(&mut chunk).await {
                    if n == 0 {
                        break;
                    }

                    let chunk = String::from_utf8_lossy(&chunk[..n]);
                    tracing::debug!(id=%stdout_task_id, "{chunk}");

                    let msg = ServerMessage::TaskIoChunk(TaskIoChunk {
                        id: stdout_task_id.clone(),
                        chunk: chunk.to_string(),
                        io_type: IoType::Stdout,
                    });

                    stdout_connection_manager.broadcast(msg);
                }

                tracing::debug!(id=%stdout_task_id, "Finished reading stdout");
            });

            tokio::spawn(async move {
                let mut chunk = [0; 256];
                while let Ok(n) = stderr_rx.read(&mut chunk).await {
                    if n == 0 {
                        break;
                    }

                    let chunk = String::from_utf8_lossy(&chunk[..n]);
                    tracing::error!(id=%stderr_task_id, "{chunk}");

                    let msg = ServerMessage::TaskIoChunk(TaskIoChunk {
                        id: stderr_task_id.clone(),
                        chunk: chunk.to_string(),
                        io_type: IoType::Stderr,
                    });

                    stderr_connection_manager.broadcast(msg);
                }

                tracing::debug!(id=%stderr_task_id, "Finished reading stderr");
            });

            task.run_os_process(timeout, Some(stdout_tx), Some(stderr_tx))
                .await;

            // Keeping task in memory for 15 minutes after it's done.
            // simulating an in-memory database.

            tracing::debug!(id=%task_id, "Task finished. Waiting 15 minutes before removing it from memory");
            tokio::time::sleep(std::time::Duration::from_secs(900)).await;
            tracing::debug!(id=%task_id, "Removing task from memory");
            let mut tasks = tasks.write().await;
            tasks.remove(&task_id);
        });

        id
    }

    /// Send a cancel signal to the task with the given id and return immediately.
    /// The Terminated task will be removed fom memory in a different tokio task which is spawned by [`ApiStateInner::run_task`].
    pub async fn cancel_task<'a>(&self, id: &'a str, chat_id: &str) -> Option<&'a str> {
        let tasks = self.tasks.read().await;
        match tasks.get(id) {
            Some(task_data) if task_data.chat_id == chat_id => {
                task_data.handle.send_cancel_signal().await;

                Some(id)
            }
            _ => None,
        }
    }

    pub async fn task_status(&self, id: &str, chat_id: &str) -> Option<Status> {
        let tasks = self.tasks.read().await;
        match tasks.get(id) {
            Some(task_data) if task_data.chat_id == chat_id => {
                let status = task_data.handle.status().await;

                Some(status)
            }
            _ => None,
        }
    }

    pub async fn list_files(&self, project_name: String) -> Result<Vec<String>, ListFilesError> {
        let project_dir = PathBuf::from(&self.projects_dir).join(project_name);

        if !project_dir.exists() {
            return Err(ListFilesError::NotFound);
        }

        let mut read_dir = tokio::fs::read_dir(project_dir).await?;

        let mut files: Vec<String> = Vec::new();

        while let Ok(Some(entry)) = read_dir.next_entry().await {
            let file_name = entry.file_name();
            let file_name = file_name.to_string_lossy().to_string();

            files.push(file_name);
        }

        Ok(files)
    }

    pub async fn get_file(
        &self,
        project_name: String,
        file_name: String,
    ) -> Result<String, GetFileError> {
        let project_dir = PathBuf::from(&self.projects_dir).join(project_name);

        if !project_dir.exists() {
            return Err(GetFileError::NotFound);
        }

        let file_path = project_dir.join(file_name);

        if !file_path.exists() {
            return Err(GetFileError::NotFound);
        }

        let file_content = tokio::fs::read_to_string(file_path).await?;

        Ok(file_content)
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ListFilesError {
    #[error("Project not found")]
    NotFound,
    #[error("IO error: {0}")]
    IoError(#[from] std::io::Error),
}

#[derive(Debug, thiserror::Error)]
pub enum GetFileError {
    #[error("Project/File not found")]
    NotFound,
    #[error("IO error: {0}")]
    IoError(#[from] std::io::Error),
}

impl Deref for ApiState {
    type Target = ApiStateInner;

    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}

impl Drop for ApiStateInner {
    fn drop(&mut self) {
        tracing::trace!("Api state inner dropped");
    }
}
