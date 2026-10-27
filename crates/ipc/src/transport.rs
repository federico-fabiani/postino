//! The two local transports: a Windows named pipe and a Unix domain socket. Both expose the same
//! byte stream; nothing above this module knows which one is in use.

use std::time::Duration;

use tokio::io::{AsyncRead, AsyncWrite};

use crate::{Endpoint, IpcError};

/// A byte stream a server or client can split.
pub trait Stream: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send> Stream for T {}

#[cfg(windows)]
mod imp {
    use std::time::Duration;

    use tokio::net::windows::named_pipe::{ClientOptions, NamedPipeServer, PipeMode, ServerOptions};
    use tokio::time::{Instant, sleep};

    use super::{IpcError, Stream};
    use crate::platform::PipeSecurity;

    const MAX_INSTANCES: usize = 254;

    pub struct Listener {
        name: String,
        next: Option<NamedPipeServer>,
    }

    impl Listener {
        pub async fn bind(endpoint: &str) -> Result<Self, IpcError> {
            let first = create(endpoint, true)?;
            Ok(Self { name: endpoint.to_owned(), next: Some(first) })
        }

        pub async fn accept(&mut self) -> Result<Box<dyn Stream>, IpcError> {
            let server = self.next.take().ok_or_else(|| IpcError::Configuration("the named pipe listener has no next instance".to_owned()))?;
            server.connect().await?;
            // Create the next instance before returning the accepted one, so a burst of clients never
            // sees a gap between connections.
            self.next = Some(create(&self.name, false)?);
            Ok(Box::new(server))
        }
    }

    impl Drop for Listener {
        fn drop(&mut self) {
            // Dropping the current instance closes the pipe name when the last instance is gone.
            self.next.take();
        }
    }

    fn create(name: &str, first: bool) -> Result<NamedPipeServer, IpcError> {
        let mut security = PipeSecurity::new().map_err(IpcError::Io)?;
        let mut options = ServerOptions::new();
        options
            .pipe_mode(PipeMode::Byte)
            .first_pipe_instance(first)
            .reject_remote_clients(true)
            .max_instances(MAX_INSTANCES)
            .in_buffer_size(64 * 1024)
            .out_buffer_size(64 * 1024);
        // SAFETY: the security attributes and their descriptor outlive this call; the descriptor has
        // one ACE for the current user only.
        #[allow(unsafe_code)]
        unsafe {
            options.create_with_security_attributes_raw(name, security.as_raw()).map_err(IpcError::Io)
        }
    }

    pub async fn connect(endpoint: &str, timeout: Duration) -> Result<Box<dyn Stream>, IpcError> {
        let deadline = Instant::now() + timeout;
        loop {
            match ClientOptions::new().open(endpoint) {
                Ok(client) => return Ok(Box::new(client)),
                Err(error)
                    if (error.kind() == std::io::ErrorKind::NotFound
                        || error.kind() == std::io::ErrorKind::WouldBlock
                        || matches!(error.raw_os_error(), Some(231 | 232)))
                        && Instant::now() < deadline =>
                {
                    sleep(Duration::from_millis(20)).await;
                }
                Err(error) => return Err(IpcError::Io(error)),
            }
        }
    }
}

#[cfg(unix)]
mod imp {
    use std::os::unix::fs::PermissionsExt as _;
    use std::path::{Path, PathBuf};

    use tokio::net::{UnixListener, UnixStream};
    use tokio::time::{Duration, Instant, sleep};

    use super::{IpcError, Stream};

    pub struct Listener {
        listener: UnixListener,
        path: PathBuf,
    }

    impl Listener {
        pub async fn bind(path: &Path) -> Result<Self, IpcError> {
            use std::os::unix::fs::DirBuilderExt as _;
            if let Some(parent) = path.parent().filter(|parent| !parent.as_os_str().is_empty()) {
                if parent.exists() {
                    crate::platform::verify_owner_only_dir(parent).map_err(IpcError::Io)?;
                } else {
                    std::fs::DirBuilder::new().mode(0o700).create_all(parent)?;
                    crate::platform::verify_owner_only_dir(parent).map_err(IpcError::Io)?;
                }
            }
            // A stale socket from a crashed daemon is safe to remove: the daemon holds the writer
            // lock, and a live listener answers a probe.
            if path.exists() {
                match std::os::unix::net::UnixStream::connect(path) {
                    Ok(_) => return Err(IpcError::Configuration(format!("another process already listens on {}", path.display()))),
                    Err(_) => {
                        std::fs::remove_file(path)?;
                    }
                }
            }
            let listener = UnixListener::bind(path)?;
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
            Ok(Self { listener, path: path.to_path_buf() })
        }

        pub async fn accept(&mut self) -> Result<Box<dyn Stream>, IpcError> {
            let (stream, _) = self.listener.accept().await?;
            // A second permission set covers a filesystem that loses the mode on bind.
            let _ = std::fs::set_permissions(&self.path, std::fs::Permissions::from_mode(0o600));
            verify_peer_uid(&stream)?;
            Ok(Box::new(stream))
        }
    }

    impl Drop for Listener {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.path);
        }
    }

    pub async fn connect(path: &Path, timeout: Duration) -> Result<Box<dyn Stream>, IpcError> {
        let deadline = Instant::now() + timeout;
        loop {
            match UnixStream::connect(path).await {
                Ok(stream) => return Ok(Box::new(stream)),
                Err(error)
                    if matches!(error.kind(), std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused) && Instant::now() < deadline =>
                {
                    sleep(Duration::from_millis(20)).await;
                }
                Err(error) => return Err(IpcError::Io(error)),
            }
        }
    }

    /// On Linux, reject a peer whose uid is not ours before any token is read. Socket mode 0600 is
    /// the primary control; this is the second check the threat model promises.
    #[cfg(target_os = "linux")]
    #[allow(unsafe_code)]
    fn verify_peer_uid(stream: &UnixStream) -> Result<(), IpcError> {
        use std::os::fd::AsRawFd as _;
        let mut credentials = libc::ucred { pid: 0, uid: 0, gid: 0 };
        let mut length = u32::try_from(std::mem::size_of::<libc::ucred>()).unwrap_or(0);
        // SAFETY: the fd is valid for the call, credentials points to a writable ucred and length is
        // its exact size.
        #[allow(unsafe_code)]
        let result = unsafe {
            libc::getsockopt(
                stream.as_raw_fd(),
                libc::SOL_SOCKET,
                libc::SO_PEERCRED,
                (&mut credentials as *mut libc::ucred).cast::<std::ffi::c_void>(),
                &mut length,
            )
        };
        if result != 0 {
            return Err(IpcError::Io(std::io::Error::last_os_error()));
        }
        // SAFETY: getuid has no preconditions.
        let uid = unsafe { libc::getuid() };
        if credentials.uid != uid {
            return Err(IpcError::Unauthorized);
        }
        Ok(())
    }

    #[cfg(not(target_os = "linux"))]
    fn verify_peer_uid(_stream: &UnixStream) -> Result<(), IpcError> {
        // macOS/BSD peer-credential APIs differ; the owner-only directory and 0600 socket remain.
        Ok(())
    }
}

#[cfg(not(any(unix, windows)))]
mod imp {
    use std::path::Path;
    use std::time::Duration;

    use super::{IpcError, Stream};

    pub struct Listener;

    impl Listener {
        pub async fn bind(_path: &str) -> Result<Self, IpcError> {
            Err(IpcError::Configuration("Postino IPC is implemented on Windows and Unix only".to_owned()))
        }

        pub async fn accept(&mut self) -> Result<Box<dyn Stream>, IpcError> {
            Err(IpcError::Configuration("Postino IPC is implemented on Windows and Unix only".to_owned()))
        }
    }

    pub async fn connect(_path: &Path, _timeout: Duration) -> Result<Box<dyn Stream>, IpcError> {
        Err(IpcError::Configuration("Postino IPC is implemented on Windows and Unix only".to_owned()))
    }
}

/// A bound local endpoint.
pub struct Listener {
    inner: imp::Listener,
}

impl Listener {
    pub async fn bind(endpoint: &Endpoint) -> Result<Self, IpcError> {
        #[cfg(windows)]
        let inner = match endpoint {
            Endpoint::NamedPipe(name) => imp::Listener::bind(name).await?,
            #[allow(unreachable_patterns)]
            _ => return Err(IpcError::Configuration("this Windows build serves named pipes only".to_owned())),
        };
        #[cfg(unix)]
        let inner = match endpoint {
            Endpoint::UnixSocket(path) => imp::Listener::bind(path.as_path()).await?,
            #[allow(unreachable_patterns)]
            _ => return Err(IpcError::Configuration("this Unix build serves domain sockets only".to_owned())),
        };
        #[cfg(not(any(unix, windows)))]
        let inner = imp::Listener::bind("")?;
        Ok(Self { inner })
    }

    pub async fn accept(&mut self) -> Result<Box<dyn Stream>, IpcError> {
        self.inner.accept().await
    }
}

/// Connect to a local endpoint, waiting up to `timeout` for a daemon that is still binding.
pub async fn connect(endpoint: &Endpoint, timeout: Duration) -> Result<Box<dyn Stream>, IpcError> {
    #[cfg(windows)]
    {
        match endpoint {
            Endpoint::NamedPipe(name) => imp::connect(name, timeout).await,
            #[allow(unreachable_patterns)]
            _ => Err(IpcError::Configuration("this Windows build connects to named pipes only".to_owned())),
        }
    }
    #[cfg(unix)]
    {
        match endpoint {
            Endpoint::UnixSocket(path) => imp::connect(path.as_path(), timeout).await,
            #[allow(unreachable_patterns)]
            _ => Err(IpcError::Configuration("this Unix build connects to domain sockets only".to_owned())),
        }
    }
    #[cfg(not(any(unix, windows)))]
    {
        imp::connect(endpoint.path(), timeout).await
    }
}
