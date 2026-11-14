use crate::execution::{ExecOutcome, ExecutionEngine};
use crate::strategies::Signal;
use anyhow::Result;

pub struct ExecutionAgent {
    engine: ExecutionEngine,
}

impl ExecutionAgent {
    pub fn new(engine: ExecutionEngine) -> Self {
        Self { engine }
    }

    pub async fn execute(&mut self, signal: Signal) -> Result<ExecOutcome> {
        self.engine.execute(signal).await
    }
}