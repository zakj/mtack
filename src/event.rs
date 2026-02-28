// App-internal event types: PTY output and process lifecycle.

use bytes::Bytes;

#[derive(Debug)]
pub enum Event {
    PtyOutput { id: usize, data: Bytes },
    ProcessExited { id: usize, status: ProcessStatus },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProcessStatus {
    Success,
    Failed(i32),
    Signal,
}
