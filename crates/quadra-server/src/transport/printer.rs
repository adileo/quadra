//! Printers: what `print(content)` gives (ZPL, text, the bytes of a PDF), sent to the printer
//! of a connection of `kind = "printer"`.
//!
//! `tcp://host:9100` is raw printing ("JetDirect"): a connection to the printer's port, the
//! bytes written as they are, the write side shut, then a moment for the printer to close it
//! (most do when they have the job; some keep it open, and the job is done all the same). Each
//! copy is a connection of its own: one job each. With `public_only` (Quadra Cloud) the host
//! must be a public address: a printer of the company's network is reached by an agent
//! (`location = "agent:<name>"`), which runs this same code in that network.
//!
//! `lp://NAME` is a printer of the system where the connection runs, through CUPS: the bytes
//! go to `lp -d NAME -n COPIES`, with `-o raw` unless they are a PDF (so that ZPL or text reach a
//! raw queue as they are, and a PDF is printed by the queue's driver). Not on Windows yet.
//!
//! A printer that refuses the connection, does not answer in the connection's `timeout`, or a
//! `lp` that fails: tried again. A url that is not a printer's: never.

use super::{Failed, Put, Resolved};
use quadra_sema::manifest::{printer_at, ConnectionKind, PrinterAt};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// How long the printer is given to close the connection once it has the job.
const LINGER: Duration = Duration::from_secs(2);

pub async fn put(c: &Resolved, put: &Put, public_only: bool) -> Result<(), Failed> {
    let Put::Print { body, content_type } = put else {
        return Err(Failed::Never(
            "a printer takes what `print(…)` gives".into(),
        ));
    };
    let ConnectionKind::Printer {
        url,
        timeout_s,
        copies,
    } = &c.connection.kind
    else {
        return Err(Failed::Never("not a printer connection".into()));
    };
    let url = c.setting(url).map_err(Failed::Waiting)?;
    let at = printer_at(&url).map_err(|e| Failed::Never(format!("`{url}`: {e}")))?;
    let limit = Duration::from_secs(*timeout_s);
    if body.is_empty() {
        return Err(Failed::Never(
            "nothing to print: the content is empty".into(),
        ));
    }
    match at {
        PrinterAt::Tcp { host, port } => {
            for _ in 0..*copies {
                raw(&host, port, body, public_only, limit).await?;
            }
            Ok(())
        }
        PrinterAt::Lp { name } => {
            if cfg!(windows) {
                return Err(Failed::Waiting(format!(
                    "`{}`: lp://{name} prints with CUPS, which Windows does not have: printing to a printer of Windows is not supported yet (a printer with a network port is reached with tcp://host:9100)",
                    c.connection.name
                )));
            }
            lp(
                "lp",
                &c.connection.name,
                &name,
                body,
                content_type,
                *copies,
                limit,
            )
            .await
        }
    }
}

/// One job to the raw port of a printer.
async fn raw(
    host: &str,
    port: u16,
    body: &[u8],
    public_only: bool,
    limit: Duration,
) -> Result<(), Failed> {
    let addrs = crate::deliver::addresses(host, port, public_only)
        .await
        .map_err(|e| {
            if public_only && e.contains("not a public address") {
                Failed::Never(format!("{e}: a printer of the company's network is reached by an on-premise agent (location = \"agent:<name>\")"))
            } else {
                Failed::Again(e)
            }
        })?;
    let mut last = String::new();
    let mut stream = None;
    for a in addrs {
        match tokio::time::timeout(limit, tokio::net::TcpStream::connect(a)).await {
            Ok(Ok(s)) => {
                stream = Some(s);
                break;
            }
            Ok(Err(e)) => last = format!("the printer {host}:{port}: {e}"),
            Err(_) => {
                last = format!(
                    "the printer {host}:{port}: no answer in {}s",
                    limit.as_secs()
                )
            }
        }
    }
    let Some(mut s) = stream else {
        return Err(Failed::Again(last));
    };
    let sent = tokio::time::timeout(limit, async {
        s.write_all(body).await?;
        s.flush().await?;
        s.shutdown().await
    })
    .await;
    match sent {
        Err(_) => {
            return Err(Failed::Again(format!(
                "the printer {host}:{port} did not take the job in {}s",
                limit.as_secs()
            )))
        }
        Ok(Err(e)) => return Err(Failed::Again(format!("the printer {host}:{port}: {e}"))),
        Ok(Ok(())) => {}
    }
    // the printer closes when it has it all; one that keeps it open has it all the same
    let mut buf = [0u8; 512];
    let _ = tokio::time::timeout(LINGER.min(limit), async {
        loop {
            match s.read(&mut buf).await {
                Ok(0) | Err(_) => break,
                Ok(_) => {}
            }
        }
    })
    .await;
    Ok(())
}

/// The arguments of `lp` for a job: the queue, the copies, `-o raw` unless it is a PDF.
fn lp_args(name: &str, copies: u32, content_type: &str) -> Vec<String> {
    let mut args = vec![
        "-d".to_string(),
        name.to_string(),
        "-n".to_string(),
        copies.to_string(),
    ];
    if !content_type.starts_with("application/pdf") {
        args.push("-o".into());
        args.push("raw".into());
    }
    args
}

/// A job given to `program` (`lp`) on its standard input.
async fn lp(
    program: &str,
    conn: &str,
    name: &str,
    body: &[u8],
    content_type: &str,
    copies: u32,
    limit: Duration,
) -> Result<(), Failed> {
    let mut child = tokio::process::Command::new(program)
        .args(lp_args(name, copies, content_type))
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                Failed::Again(format!("`{conn}`: `lp` is not installed where it runs: lp://{name} prints with CUPS (`lp -d {name}`)"))
            } else {
                Failed::Again(format!("`{conn}`: `lp` did not start: {e}"))
            }
        })?;
    let mut stdin = child.stdin.take().expect("piped");
    let body = body.to_vec();
    let writing = tokio::spawn(async move {
        let r = stdin.write_all(&body).await;
        drop(stdin);
        r
    });
    let out = tokio::time::timeout(limit, child.wait_with_output())
        .await
        .map_err(|_| {
            Failed::Again(format!(
                "`{conn}`: lp -d {name} did not finish in {}s",
                limit.as_secs()
            ))
        })?
        .map_err(|e| Failed::Again(format!("`{conn}`: lp -d {name}: {e}")))?;
    let wrote = writing
        .await
        .unwrap_or_else(|e| Err(std::io::Error::other(e)));
    if !out.status.success() {
        let said = String::from_utf8_lossy(&out.stderr).trim().to_string();
        return Err(Failed::Again(format!(
            "`{conn}`: lp -d {name} failed ({}){}",
            out.status,
            if said.is_empty() {
                String::new()
            } else {
                format!(": {said}")
            }
        )));
    }
    wrote.map_err(|e| Failed::Again(format!("`{conn}`: lp -d {name} did not take the job: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn resolved(decl: &str) -> Resolved {
        let m = quadra_sema::manifest::parse(&format!("[connections.zebra]\n{decl}")).unwrap();
        super::super::Conn::of(&m, "zebra")
            .unwrap()
            .open(&BTreeMap::new())
            .unwrap()
    }

    fn job(body: &[u8], content_type: &str) -> Put {
        Put::Print {
            body: body.to_vec(),
            content_type: content_type.into(),
        }
    }

    #[test]
    fn a_pdf_goes_to_the_queues_driver_and_text_to_it_raw() {
        assert_eq!(
            lp_args("Office", 2, "application/pdf"),
            ["-d", "Office", "-n", "2"]
        );
        assert_eq!(
            lp_args("Zebra", 1, "text/plain"),
            ["-d", "Zebra", "-n", "1", "-o", "raw"]
        );
    }

    #[tokio::test]
    async fn each_copy_is_a_job_of_its_own_on_the_raw_port() {
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = l.local_addr().unwrap().port();
        let got = tokio::spawn(async move {
            let mut jobs = Vec::new();
            for _ in 0..2 {
                let (mut s, _) = l.accept().await.unwrap();
                let mut b = Vec::new();
                s.read_to_end(&mut b).await.unwrap();
                jobs.push(b);
            }
            jobs
        });
        let c = resolved(&format!(
            "kind = \"printer\"\nurl = \"tcp://127.0.0.1:{port}\"\ncopies = 2\ntimeout = \"5s\"\n"
        ));
        put(&c, &job(b"^XA^FDhi^FS^XZ", "text/plain"), false)
            .await
            .unwrap();
        assert_eq!(got.await.unwrap(), [b"^XA^FDhi^FS^XZ", b"^XA^FDhi^FS^XZ"]);
    }

    #[tokio::test]
    async fn a_printer_down_is_tried_again_and_a_private_one_is_for_an_agent() {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = l.local_addr().unwrap().port();
        drop(l);
        let c = resolved(&format!(
            "kind = \"printer\"\nurl = \"tcp://127.0.0.1:{port}\"\ntimeout = \"2s\"\n"
        ));
        let e = put(&c, &job(b"x", "text/plain"), false).await.unwrap_err();
        assert!(matches!(e, Failed::Again(_)), "{e:?}");
        let e = put(&c, &job(b"x", "text/plain"), true).await.unwrap_err();
        assert!(
            matches!(&e, Failed::Never(t) if t.contains("agent")),
            "{e:?}"
        );
    }

    #[tokio::test]
    async fn a_url_that_is_not_a_printers_is_never_tried_again() {
        let m = quadra_sema::manifest::parse(
            "[vars]\nAT = {}\n\n[connections.zebra]\nkind = \"printer\"\nurl = { var = \"AT\" }\n",
        )
        .unwrap();
        let mut env = BTreeMap::new();
        env.insert("AT".to_string(), "http://printer.lan".to_string());
        let c = super::super::Conn::of(&m, "zebra")
            .unwrap()
            .open(&env)
            .unwrap();
        let e = put(&c, &job(b"x", "text/plain"), false).await.unwrap_err();
        assert!(
            matches!(&e, Failed::Never(t) if t.contains("tcp://")),
            "{e:?}"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn lp_is_given_the_bytes_and_said_when_missing_or_failing() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("quadra-printer-lp-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let fake = dir.join("lp");
        std::fs::write(
            &fake,
            format!(
                "#!/bin/sh\necho \"$@\" > {0}/args\ncat > {0}/body\n",
                dir.display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
        let pdf = b"%PDF-1.7\n\x00\xff binary\n%%EOF";
        lp(
            fake.to_str().unwrap(),
            "office",
            "Office",
            pdf,
            "application/pdf",
            2,
            Duration::from_secs(5),
        )
        .await
        .unwrap();
        assert_eq!(
            std::fs::read_to_string(dir.join("args")).unwrap().trim(),
            "-d Office -n 2"
        );
        assert_eq!(std::fs::read(dir.join("body")).unwrap(), pdf);
        // not installed: said, and tried again
        let e = lp(
            dir.join("none").to_str().unwrap(),
            "office",
            "Office",
            b"x",
            "text/plain",
            1,
            Duration::from_secs(5),
        )
        .await
        .unwrap_err();
        assert!(
            matches!(&e, Failed::Again(t) if t.contains("not installed")),
            "{e:?}"
        );
        // a queue that does not exist: what lp said
        let failing = dir.join("lp-fails");
        std::fs::write(
            &failing,
            "#!/bin/sh\ncat > /dev/null\necho 'lp: The printer or class does not exist.' >&2\nexit 1\n",
        )
        .unwrap();
        std::fs::set_permissions(&failing, std::fs::Permissions::from_mode(0o755)).unwrap();
        let e = lp(
            failing.to_str().unwrap(),
            "office",
            "Nope",
            b"x",
            "text/plain",
            1,
            Duration::from_secs(5),
        )
        .await
        .unwrap_err();
        assert!(
            matches!(&e, Failed::Again(t) if t.contains("does not exist")),
            "{e:?}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
