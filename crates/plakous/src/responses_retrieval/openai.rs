use super::{
    Arc, ExecutionOperationControl, RetrievalFuture, RetrievalKind, RetrievalPort,
    RetrievalRequest, run,
};

/// One session's OpenAI API hosted web search as a [`RetrievalPort`].
pub struct OpenAiRetrievalPort(Arc<plakous_openai::OpenAiRetrieval>);

impl OpenAiRetrievalPort {
    /// Wraps the session's side-request executor.
    #[must_use]
    pub fn new(retrieval: plakous_openai::OpenAiRetrieval) -> Self {
        Self(Arc::new(retrieval))
    }
}

impl RetrievalPort for OpenAiRetrievalPort {
    fn kinds(&self) -> &[RetrievalKind] {
        &[RetrievalKind::WebSearch]
    }

    fn retrieve(
        &self,
        request: RetrievalRequest,
        control: ExecutionOperationControl,
    ) -> RetrievalFuture {
        let retrieval = Arc::clone(&self.0);
        run(self.kinds(), request, move |query| async move {
            retrieval.retrieve(query, control.cancellation()).await
        })
    }
}
