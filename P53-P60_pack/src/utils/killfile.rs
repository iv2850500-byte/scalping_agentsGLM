//! [P11:kill] Kill-switch via KILL file in the bot working directory.
//! File exists -> new entries are rejected (exits and position management
//! keep working). Delete the file to resume. Cheap: one stat call per check.
//!
//! ✅ P54/C8-1: fail-CLOSED. Раньше Path::exists() на ЛЮБУЮ ошибку FS
//! (права, сетевой диск недоступен, ...) возвращал false → KILL молча
//! игнорировался в самый нужный момент. Теперь: файл есть → KILL;
//! NotFound → нет KILL; ЛЮБАЯ другая ошибка чтения → KILL СЧИТАЕТСЯ
//! АКТИВНЫМ (входы запрещены), ошибка логируется.
pub fn kill_switch_active() -> bool {
    match std::fs::metadata("KILL") {
        Ok(_) => true,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => false,
        Err(e) => {
            tracing::error!(
                "KILL file check FAILED ({}): fail-closed — входы считаются ЗАПРЕЩЁННЫМИ",
                e
            );
            true
        }
    }
}