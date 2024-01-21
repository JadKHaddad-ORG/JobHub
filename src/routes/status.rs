use crate::server::{state::ApiState, task::Status};
use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use serde::Serialize;
use utoipa::ToSchema;

#[derive(Serialize, ToSchema)]
pub struct StatusReponse {
    /// Status of a given task
    status: Status,
}

impl IntoResponse for StatusReponse {
    fn into_response(self) -> Response {
        (StatusCode::OK, Json(self)).into_response()
    }
}

#[utoipa::path(
    get,
    path = "/api/status/{id}", 
    params(
        ("id" = String, Path, description = "Task id")
    ),
    tag = "task",
    responses(
        (status = 200, description = "Status of a given task", body = StatusReponse, example = json!(StatusReponse{status: Status::Running})),
        (status = 404, description = "Task not found"),
        (status = 403, description = "Chat id invalid. You are trying to access resources that are not yours"),
        (status = 400, description = "Chat id missing"),
    ),
    security(
        ("api_key" = []),
        ("chat_id" = [])
    ),
)]
pub async fn status(
    State(state): State<ApiState>,
    Path(id): Path<String>,
) -> Result<StatusReponse, StatusCode> {
    let status = state.task_status(&id).await.ok_or(StatusCode::NOT_FOUND)?;

    Ok(StatusReponse { status })
}
