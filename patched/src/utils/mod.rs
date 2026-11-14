pub mod retry;
pub mod math;
pub mod rate_limiter;
// [P11:mod] kill-switch + telegram alerts
pub mod killfile;
pub mod telegram;
// P64/: типизированные ошибки приложения (E5b) + exit-коды для supervisor.ps1
pub mod app_error;
