use serde::Serialize;
use std::sync::Arc;

pub const EVENT: &str = "cswitch://operation-progress";
pub const PROVIDERS_CHANGED_EVENT: &str = "cswitch://providers-changed";
pub const OPERATION_ERROR_EVENT: &str = "cswitch://operation-error";

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct OperationProgress {
    pub operation: String,
    pub title: String,
    pub stage: String,
    pub detail: String,
    pub current: u32,
    pub total: u32,
    pub done: bool,
}

#[derive(Clone, Default)]
pub struct ProgressReporter {
    operation: String,
    title: String,
    emit: Option<Arc<dyn Fn(OperationProgress) + Send + Sync>>,
}

impl ProgressReporter {
    pub fn for_operation(
        operation: impl Into<String>,
        title: impl Into<String>,
        emit: impl Fn(OperationProgress) + Send + Sync + 'static,
    ) -> Self {
        Self {
            operation: operation.into(),
            title: title.into(),
            emit: Some(Arc::new(emit)),
        }
    }

    pub fn stage(&self, current: u32, total: u32, stage: &str, detail: &str) {
        self.push(OperationProgress {
            operation: self.operation.clone(),
            title: self.title.clone(),
            stage: stage.to_string(),
            detail: detail.to_string(),
            current,
            total,
            done: false,
        });
    }

    pub fn finish(&self, total: u32) {
        self.push(OperationProgress {
            operation: self.operation.clone(),
            title: self.title.clone(),
            stage: "完成".to_string(),
            detail: String::new(),
            current: total,
            total,
            done: true,
        });
    }

    fn push(&self, payload: OperationProgress) {
        if let Some(emit) = &self.emit {
            emit(payload);
        }
    }
}
