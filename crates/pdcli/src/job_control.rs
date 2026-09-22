#[cfg(unix)]
pub fn install() {
    let Some(settings) = tty_settings(libc::STDIN_FILENO) else {
        return;
    };
    let Ok(mut resumed) =
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::from_raw(libc::SIGCONT))
    else {
        return;
    };
    tokio::spawn(async move {
        while resumed.recv().await.is_some() {
            restore(libc::STDIN_FILENO, &settings);
        }
    });
}

#[cfg(not(unix))]
pub fn install() {}

#[cfg(unix)]
fn tty_settings(fd: libc::c_int) -> Option<libc::termios> {
    let mut settings = std::mem::MaybeUninit::uninit();
    if unsafe { libc::isatty(fd) == 1 && libc::tcgetattr(fd, settings.as_mut_ptr()) == 0 } {
        Some(unsafe { settings.assume_init() })
    } else {
        None
    }
}

#[cfg(unix)]
fn restore(fd: libc::c_int, settings: &libc::termios) {
    unsafe {
        libc::tcsetattr(fd, libc::TCSANOW, settings);
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::{restore, tty_settings};

    #[test]
    fn restores_canonical_input_and_echo() {
        let (mut master, mut slave) = (0, 0);
        assert_eq!(
            unsafe {
                libc::openpty(
                    &mut master,
                    &mut slave,
                    std::ptr::null_mut(),
                    std::ptr::null(),
                    std::ptr::null(),
                )
            },
            0
        );
        let original = tty_settings(slave).unwrap();
        let mut changed = original;
        changed.c_lflag &= !(libc::ICANON | libc::ECHO);
        restore(slave, &changed);
        restore(slave, &original);
        let restored = tty_settings(slave).unwrap();
        assert_eq!(
            restored.c_lflag & (libc::ICANON | libc::ECHO),
            original.c_lflag & (libc::ICANON | libc::ECHO)
        );
        unsafe {
            libc::close(master);
            libc::close(slave);
        }
    }
}
