use std::fmt;

#[derive(Debug)]
pub enum CofferError {
    Config(String),
    Firecracker(String),
    Network(String),
    Pool(String),
    TemplateNotFound(String),
    TemplateBuild(String),
    AgentNotReady(String),
    AgentCommunication(String),
    AgentExec { message: String, exit_code: Option<i32> },
    Io(std::io::Error),
    Serde(serde_json::Error),
    TaskJoin(String),
    Timeout(String),
}

impl fmt::Display for CofferError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CofferError::Config(s) => write!(f, "config error: {}", s),
            CofferError::Firecracker(s) => write!(f, "firecracker error: {}", s),
            CofferError::Network(s) => write!(f, "network error: {}", s),
            CofferError::Pool(s) => write!(f, "pool error: {}", s),
            CofferError::TemplateNotFound(id) => write!(f, "template not found: {}", id),
            CofferError::TemplateBuild(s) => write!(f, "template build error: {}", s),
            CofferError::AgentNotReady(id) => write!(f, "agent not ready in sandbox: {}", id),
            CofferError::AgentCommunication(s) => write!(f, "agent communication error: {}", s),
            CofferError::AgentExec { message, .. } => write!(f, "agent exec error: {}", message),
            CofferError::Io(e) => write!(f, "io error: {}", e),
            CofferError::Serde(e) => write!(f, "serde error: {}", e),
            CofferError::TaskJoin(s) => write!(f, "task join error: {}", s),
            CofferError::Timeout(s) => write!(f, "timeout: {}", s),
        }
    }
}

impl std::error::Error for CofferError {}

impl From<std::io::Error> for CofferError {
    fn from(e: std::io::Error) -> Self {
        CofferError::Io(e)
    }
}

impl From<serde_json::Error> for CofferError {
    fn from(e: serde_json::Error) -> Self {
        CofferError::Serde(e)
    }
}

pub type Result<T> = std::result::Result<T, CofferError>;
