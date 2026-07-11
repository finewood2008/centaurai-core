use axum::http::StatusCode;

#[derive(Debug, thiserror::Error)]
pub enum KnowledgeConfigError {
    #[error("knowledge worker URL must use http and a literal loopback IP")]
    NonLoopbackEndpoint,
    #[error("knowledge worker URL must not contain credentials, query, fragment, or a path")]
    InvalidEndpointShape,
    #[error("knowledge worker URL is invalid")]
    InvalidEndpoint,
    #[error("knowledge worker Unix socket path must be absolute")]
    InvalidSocketPath,
    #[error("failed to initialize knowledge worker transport")]
    TransportInitialization,
    #[error("managed knowledge worker binary path must be absolute")]
    InvalidWorkerBinaryPath,
    #[error("managed knowledge worker binary is unavailable or not executable")]
    WorkerBinaryUnavailable,
    #[error("managed knowledge worker requires an internal token")]
    MissingInternalToken,
    #[error("managed knowledge worker data directory must be absolute")]
    InvalidKnowledgeDataDir,
    #[error("managed knowledge worker lifecycle is unsupported on this platform")]
    ManagedWorkerUnsupported,
}

#[derive(Debug, thiserror::Error)]
pub enum KnowledgeLifecycleError {
    #[error("invalid managed knowledge worker configuration")]
    Config(#[from] KnowledgeConfigError),
    #[error("failed to prepare managed knowledge worker directories")]
    Prepare(#[source] std::io::Error),
    #[error("managed knowledge worker socket path is not a socket")]
    UnsafeSocketPath,
    #[error("managed knowledge worker socket is already served by another process")]
    SocketInUse,
    #[error("managed knowledge worker did not become ready")]
    ReadinessTimeout,
    #[error("managed knowledge worker supervisor stopped before readiness")]
    SupervisorStopped,
    #[error("managed knowledge worker supervisor task failed")]
    SupervisorTask,
}

#[derive(Debug, thiserror::Error)]
pub enum KnowledgeError {
    #[error("knowledge worker is not configured")]
    NotConfigured,
    #[error("knowledge worker is unavailable")]
    Unavailable,
    #[error("knowledge worker request timed out")]
    Timeout,
    #[error("knowledge worker returned an invalid response")]
    InvalidResponse,
    #[error("invalid knowledge request")]
    InvalidRequest,
    #[error("knowledge resource was not found")]
    NotFound,
    #[error("knowledge operation conflicts with current state")]
    Conflict,
    #[error("knowledge payload is too large")]
    PayloadTooLarge,
    #[error("knowledge space cloud consent is required")]
    CloudConsentRequired,
    #[error("required knowledge retrieval returned no results")]
    NoResults,
}

impl KnowledgeError {
    pub fn status_code(&self) -> StatusCode {
        match self {
            Self::InvalidRequest => StatusCode::BAD_REQUEST,
            Self::NotFound => StatusCode::NOT_FOUND,
            Self::Conflict | Self::CloudConsentRequired => StatusCode::CONFLICT,
            Self::PayloadTooLarge => StatusCode::PAYLOAD_TOO_LARGE,
            Self::NoResults => StatusCode::UNPROCESSABLE_ENTITY,
            Self::Timeout => StatusCode::GATEWAY_TIMEOUT,
            Self::InvalidResponse => StatusCode::BAD_GATEWAY,
            Self::NotConfigured | Self::Unavailable => StatusCode::SERVICE_UNAVAILABLE,
        }
    }

    pub fn error_code(&self) -> &'static str {
        match self {
            Self::NotConfigured => "KNOWLEDGE_NOT_CONFIGURED",
            Self::Unavailable => "KNOWLEDGE_WORKER_UNAVAILABLE",
            Self::Timeout => "KNOWLEDGE_WORKER_TIMEOUT",
            Self::InvalidResponse => "KNOWLEDGE_WORKER_INVALID_RESPONSE",
            Self::InvalidRequest => "KNOWLEDGE_INVALID_REQUEST",
            Self::NotFound => "KNOWLEDGE_NOT_FOUND",
            Self::Conflict => "KNOWLEDGE_CONFLICT",
            Self::PayloadTooLarge => "KNOWLEDGE_PAYLOAD_TOO_LARGE",
            Self::CloudConsentRequired => "KNOWLEDGE_CLOUD_CONSENT_REQUIRED",
            Self::NoResults => "KNOWLEDGE_NO_RESULTS",
        }
    }

    pub fn public_message(&self) -> &'static str {
        match self {
            Self::NotConfigured | Self::Unavailable => "Knowledge service is unavailable.",
            Self::Timeout => "Knowledge service timed out.",
            Self::InvalidResponse => "Knowledge service returned an incompatible response.",
            Self::InvalidRequest => "Invalid knowledge request.",
            Self::NotFound => "Knowledge resource was not found.",
            Self::Conflict => "Knowledge operation conflicts with the current state.",
            Self::PayloadTooLarge => "Knowledge payload is too large.",
            Self::CloudConsentRequired => "Allow the selected knowledge spaces to be used with cloud models first.",
            Self::NoResults => "No relevant knowledge was found.",
        }
    }
}
