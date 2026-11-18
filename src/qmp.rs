use std::{io::ErrorKind, path::Path, time::Duration};

use anyhow::{Context, Result, bail, ensure};
use serde_json::{Value, json};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::UnixStream,
    time::timeout,
};

pub struct Qmp {
    stream: BufReader<UnixStream>,
    next_id: u64,
}

impl Qmp {
    pub async fn connect(path: &Path, expected_name: &str) -> Result<Option<Self>> {
        let stream = match timeout(Duration::from_secs(3), UnixStream::connect(path)).await? {
            Ok(stream) => stream,
            Err(error)
                if matches!(
                    error.kind(),
                    ErrorKind::NotFound | ErrorKind::ConnectionRefused
                ) =>
            {
                return Ok(None);
            }
            Err(error) => return Err(error).context("connect to QMP"),
        };
        let mut qmp = Self {
            stream: BufReader::new(stream),
            next_id: 0,
        };
        ensure!(
            qmp.read().await?.get("QMP").is_some(),
            "invalid QMP greeting"
        );
        qmp.execute("qmp_capabilities").await?;
        let name = qmp.execute("query-name").await?;
        ensure!(
            name["name"].as_str() == Some(expected_name),
            "QMP socket belongs to another VM; refusing to control it"
        );
        Ok(Some(qmp))
    }

    async fn read(&mut self) -> Result<Value> {
        let mut line = String::new();
        let bytes = timeout(Duration::from_secs(3), self.stream.read_line(&mut line))
            .await
            .context("QMP response timed out")??;
        if bytes == 0 {
            return Err(
                std::io::Error::new(ErrorKind::UnexpectedEof, "QMP connection closed").into(),
            );
        }
        serde_json::from_str(&line).context("decode QMP response")
    }

    pub async fn execute(&mut self, command: &str) -> Result<Value> {
        let id = self.next_id;
        self.next_id += 1;
        let mut bytes = serde_json::to_vec(&json!({"execute": command, "id": id}))?;
        bytes.push(b'\n');
        timeout(
            Duration::from_secs(3),
            self.stream.get_mut().write_all(&bytes),
        )
        .await??;
        timeout(Duration::from_secs(5), async {
            loop {
                let response = match self.read().await {
                    Ok(response) => response,
                    Err(error)
                        if command == "quit"
                            && error
                                .downcast_ref::<std::io::Error>()
                                .is_some_and(|error| error.kind() == ErrorKind::UnexpectedEof) =>
                    {
                        return Ok(Value::Null);
                    }
                    Err(error) => return Err(error),
                };
                if response.get("event").is_some() {
                    continue;
                }
                ensure!(
                    response["id"].as_u64() == Some(id),
                    "unexpected QMP response id"
                );
                if let Some(error) = response.get("error") {
                    bail!("QMP {command}: {error}");
                }
                return response
                    .get("return")
                    .cloned()
                    .context("QMP response missing result");
            }
        })
        .await
        .context("QMP command timed out")?
    }
}
