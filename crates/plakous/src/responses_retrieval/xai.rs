use super::{
    Arc, OperationContext, RetrievalFuture, RetrievalKind, RetrievalPort, RetrievalRequest, run,
};

/// One session's xAI API or SuperGrok hosted web and X search as a
/// [`RetrievalPort`].
pub struct XaiRetrievalPort(Arc<plakous_xai::XaiRetrieval>);

impl XaiRetrievalPort {
    /// Wraps the session's side-request executor.
    #[must_use]
    pub fn new(retrieval: plakous_xai::XaiRetrieval) -> Self {
        Self(Arc::new(retrieval))
    }
}

impl RetrievalPort for XaiRetrievalPort {
    fn kinds(&self) -> &[RetrievalKind] {
        &[RetrievalKind::WebSearch, RetrievalKind::PostSearch]
    }

    fn retrieve(&self, request: RetrievalRequest, control: OperationContext) -> RetrievalFuture {
        let retrieval = Arc::clone(&self.0);
        run(self.kinds(), request, move |query| async move {
            retrieval.retrieve(query, control.cancellation()).await
        })
    }
}
