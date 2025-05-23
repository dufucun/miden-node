use std::fmt::Debug;

use axum::{
    http::{StatusCode, header},
    response::{IntoResponse, Response},
};
use miden_objects::AccountIdError;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum ClientError {
    #[error(transparent)]
    RequestError(#[from] tonic::Status),

    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

#[derive(Debug, Error)]
pub enum HandlerError {
    #[error("client error")]
    ClientError(#[from] ClientError),

    #[error("internal error")]
    Internal(#[from] anyhow::Error),

    #[error("account ID deserialization failed")]
    AccountIdDeserializationError(#[source] AccountIdError),

    #[error("invalid asset amount {requested} requested, valid options are {options:?}")]
    InvalidAssetAmount { requested: u64, options: Vec<u64> },
}

impl HandlerError {
    fn status_code(&self) -> StatusCode {
        match *self {
            Self::InvalidAssetAmount { .. } | Self::AccountIdDeserializationError(_) => {
                StatusCode::BAD_REQUEST
            },
            Self::ClientError(_) | Self::Internal(_) => StatusCode::INTERNAL_SERVER_ERROR,
        }
    }

    fn message(self) -> String {
        match self {
            Self::ClientError(_) | Self::Internal(_) => "Internal error".to_string(),
            other => format!("{:#}", anyhow::Error::new(other)),
        }
    }
}

impl IntoResponse for HandlerError {
    fn into_response(self) -> Response {
        (
            self.status_code(),
            [(header::CONTENT_TYPE, mime::TEXT_HTML_UTF_8.as_ref())],
            self.message(),
        )
            .into_response()
    }
}
