//! Test helpers shared across capabilities.
//!
//! Capability-specific fixtures stay local; `notifications` builds its body span in its own
//! `test_support`. This file holds [`p2p_pair`] and [`private_bus`], rather than duplicating or
//! reaching across modules.
//!
//! `pub(crate)` rather than `pub(super)`, because `polkit` is a D-Bus test outside `capabilities`
//! and needs the same pair. A second copy there is what this file exists to prevent.

use tokio::io::AsyncBufReadExt;
use tokio::net::UnixStream;

/// Connected p2p zbus connections without a bus daemon, returned as `(server, client)`. `serve`
/// installs the client's interfaces; pass `Ok` when the client answers nothing, which is most tests
/// here, since binding a proxy makes no call unless it asks for `CacheProperties::Yes` -- that one
/// fetches the cache inside `ProxyBuilder::build` and does need an answering peer.
///
/// Installing them on the builder rather than through `client.object_server().at(..)` is the whole
/// point. `Connection::object_server()` starts the dispatch task without waiting for it to
/// subscribe, and the socket reader is running by then, so a call arriving in that window is
/// broadcast to no receiver and dropped -- not delayed, dropped, which no timeout can rescue.
/// `Builder::build` is the ordering that holds: it adds the interfaces, starts dispatch, awaits its
/// started event, and only then calls `init_socket_reader`, with zbus's own comment there saying
/// the reader would otherwise lose early messages. That race cost `tray`'s two answering tests
/// their first call about one loaded `just check` in five, and a throwaway warm-up call papered
/// over it in one of them for a while.
///
/// Both sides get a `method_timeout` only as a backstop, because zbus's default is `None` and a
/// call nobody answers then waits for ever, wedging the suite instead of failing it. Nothing here
/// should come near five seconds.
pub(crate) async fn p2p_pair() -> (zbus::Connection, zbus::Connection) {
    p2p_pair_serving(Ok).await
}

/// [`p2p_pair`] with stub interfaces installed on the client, which is where its doc explains why
/// they have to go in here rather than through `object_server().at(..)` afterwards.
pub(crate) async fn p2p_pair_serving<F>(serve: F) -> (zbus::Connection, zbus::Connection)
where
    F: FnOnce(zbus::connection::Builder<'static>) -> zbus::Result<zbus::connection::Builder<'static>>,
{
    let (a, b) = UnixStream::pair().expect("failed to create a unix socket pair");
    let guid = zbus::Guid::generate();
    let backstop = std::time::Duration::from_secs(5);
    let server_builder = zbus::connection::Builder::unix_stream(a)
        .server(guid)
        .expect("p2p server builder setup")
        .p2p()
        .method_timeout(backstop);
    let client_builder = serve(zbus::connection::Builder::unix_stream(b).p2p().method_timeout(backstop))
        .expect("failed to install the client's interfaces");
    tokio::try_join!(server_builder.build(), client_builder.build()).expect("p2p handshake")
}

/// A `dbus-daemon` of this test's own, for tests that need a bus's `NameOwnerChanged`: a service
/// vanishing and coming back, which a [`p2p_pair`] has no bus to report. It has no `<servicedir>`,
/// so a proxy to a name nobody owns fails instead of D-Bus-activating the host's real daemon, which
/// the stock session config would do.
pub(crate) struct PrivateBus {
    address: String,
    _daemon: tokio::process::Child,
    _dir: tempfile::TempDir,
}

impl PrivateBus {
    /// A builder on this bus with [`p2p_pair`]'s five-second backstop, for a test to add a name or
    /// interfaces to before `build`.
    pub(crate) fn builder(&self) -> zbus::connection::Builder<'static> {
        zbus::connection::Builder::address(self.address.as_str())
            .expect("dbus-daemon printed a valid address")
            .method_timeout(std::time::Duration::from_secs(5))
    }

    /// A plain client connection to this bus.
    pub(crate) async fn connection(&self) -> zbus::Connection {
        self.builder().build().await.expect("connect to the private bus")
    }
}

/// Starts a [`PrivateBus`]. Panics without `dbus-daemon` rather than skipping, since a skipped
/// restart test reads as a passing one.
pub(crate) async fn private_bus() -> PrivateBus {
    let dir = tempfile::tempdir().expect("temp dir for the private bus");
    let config = dir.path().join("bus.conf");
    std::fs::write(
        &config,
        format!(
            r#"<!DOCTYPE busconfig PUBLIC "-//freedesktop//DTD D-Bus Bus Configuration 1.0//EN"
 "http://www.freedesktop.org/standards/dbus/1.0/busconfig.dtd">
<busconfig>
  <type>session</type>
  <listen>unix:dir={}</listen>
  <auth>EXTERNAL</auth>
  <policy context="default"><allow send_destination="*"/><allow receive_sender="*"/><allow own="*"/></policy>
</busconfig>"#,
            dir.path().display()
        ),
    )
    .expect("write the private bus config");
    let mut daemon = tokio::process::Command::new("dbus-daemon")
        .arg(format!("--config-file={}", config.display()))
        .args(["--nofork", "--print-address"])
        .stdout(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .expect("dbus-daemon must be installed: the bus restart tests need a private bus");
    let mut address = String::new();
    tokio::io::BufReader::new(daemon.stdout.take().expect("piped stdout"))
        .read_line(&mut address)
        .await
        .expect("read the private bus address");
    assert!(!address.trim().is_empty(), "dbus-daemon exited without printing an address");
    PrivateBus { address: address.trim().to_string(), _daemon: daemon, _dir: dir }
}

/// Awaits `future` for two seconds, for a bus event that must arrive; a missing one fails the test
/// instead of wedging it.
pub(crate) async fn within<T>(future: impl std::future::Future<Output = T>) -> T {
    tokio::time::timeout(std::time::Duration::from_secs(2), future).await.expect("no bus event within two seconds")
}

#[cfg(test)]
mod tests {
    /// `#[zbus::interface]` copies an item's doc comment into the introspection XML as an XML
    /// comment, where a double hyphen is forbidden, so the house em-dash breaks every conforming
    /// client. A file exporting an interface therefore spells the dash some other way, which is
    /// one rule for a whole file rather than a judgement about which items reach the XML.
    #[test]
    fn no_doc_comment_beside_an_interface_can_close_the_xml_comment_around_it() {
        let mut paths = vec![std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src")];
        let mut offences = Vec::new();
        while let Some(path) = paths.pop() {
            if path.is_dir() {
                let entries = std::fs::read_dir(&path).expect("readable source directory");
                paths.extend(entries.map(|entry| entry.expect("readable source entry").path()));
                continue;
            }
            let source = std::fs::read_to_string(&path).expect("readable source");
            // Only at the start of a line: the attribute also appears inside this test's own strings.
            let lines: Vec<&str> = source.lines().map(str::trim_start).collect();
            if !lines.iter().any(|line| line.starts_with("#[zbus::interface") || line.starts_with("#[interface")) {
                continue;
            }
            for (offset, line) in lines.iter().enumerate() {
                if line.starts_with("///") && line.contains("--") {
                    offences.push(format!("{}:{}: {line}", path.display(), offset + 1));
                }
            }
        }
        assert!(
            offences.is_empty(),
            "a file exporting a D-Bus interface cannot use `--` in a doc comment:\n{}",
            offences.join("\n")
        );
    }
}
