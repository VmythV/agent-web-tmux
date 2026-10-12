use crate::tmux::{TmuxSession, terminal_size};
use anyhow::{Context, Result};
use portable_pty::{Child, CommandBuilder, MasterPty, PtySize, native_pty_system};
use std::{
    io::{Read, Write},
    thread,
};
use tokio::sync::mpsc;

pub enum Output {
    Data(String),
    End,
}

pub struct Terminal {
    master: Box<dyn MasterPty + Send>,
    child: Option<Box<dyn Child + Send + Sync>>,
    writer: mpsc::Sender<Vec<u8>>,
}

impl Terminal {
    pub fn attach(
        session: &TmuxSession,
        cols: u16,
        rows: u16,
    ) -> Result<(Self, mpsc::Receiver<Output>)> {
        terminal_size(cols, rows)?;
        let pair = native_pty_system().openpty(size(cols, rows))?;
        let mut command = CommandBuilder::new(&session.tmux_bin);
        command.args(session.attach_args());
        command.cwd(&session.workdir);
        command.env("TERM", "xterm-256color");
        command.env("COLORTERM", "truecolor");
        command.env_remove("TMUX");
        let child = pair
            .slave
            .spawn_command(command)
            .context("无法连接 tmux 终端")?;
        drop(pair.slave);
        let mut reader = pair.master.try_clone_reader()?;
        let mut writer = pair.master.take_writer()?;
        let (output_tx, output_rx) = mpsc::channel(64);
        let (writer_tx, mut writer_rx) = mpsc::channel::<Vec<u8>>(32);

        // PTY descriptors are blocking. Dedicated threads keep the async HTTP server responsive.
        thread::spawn(move || {
            let mut bytes = [0u8; 8192];
            let mut pending = Vec::new();
            loop {
                match reader.read(&mut bytes) {
                    Ok(0) | Err(_) => break,
                    Ok(count) => {
                        pending.extend_from_slice(&bytes[..count]);
                        let text = decode_utf8(&mut pending, false);
                        if !text.is_empty() && output_tx.blocking_send(Output::Data(text)).is_err()
                        {
                            return;
                        }
                    }
                }
            }
            let tail = decode_utf8(&mut pending, true);
            if !tail.is_empty() {
                let _ = output_tx.blocking_send(Output::Data(tail));
            }
            let _ = output_tx.blocking_send(Output::End);
        });
        thread::spawn(move || {
            while let Some(bytes) = writer_rx.blocking_recv() {
                if writer
                    .write_all(&bytes)
                    .and_then(|_| writer.flush())
                    .is_err()
                {
                    break;
                }
            }
        });
        Ok((
            Self {
                master: pair.master,
                child: Some(child),
                writer: writer_tx,
            },
            output_rx,
        ))
    }

    pub fn write(&self, text: &str) -> Result<()> {
        self.writer
            .try_send(text.as_bytes().to_vec())
            .context("终端输入队列已满或连接已关闭")
    }

    pub fn resize(&self, cols: u16, rows: u16) -> Result<()> {
        terminal_size(cols, rows)?;
        self.master.resize(size(cols, rows))
    }
}

impl Drop for Terminal {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() {
            // Kill only the attach client. The agent belongs to the independent tmux server.
            let _ = child.kill();
            thread::spawn(move || {
                let _ = child.wait();
            });
        }
    }
}

fn size(cols: u16, rows: u16) -> PtySize {
    PtySize {
        cols,
        rows,
        pixel_width: 0,
        pixel_height: 0,
    }
}

// A read may split a Chinese character in the middle of its UTF-8 bytes.
fn decode_utf8(pending: &mut Vec<u8>, eof: bool) -> String {
    let mut text = String::new();
    loop {
        match std::str::from_utf8(pending) {
            Ok(valid) => {
                text.push_str(valid);
                pending.clear();
                break;
            }
            Err(error) => {
                let valid = error.valid_up_to();
                text.push_str(
                    std::str::from_utf8(&pending[..valid]).expect("validated UTF-8 prefix"),
                );
                pending.drain(..valid);
                if let Some(invalid) = error.error_len() {
                    text.push('\u{fffd}');
                    pending.drain(..invalid);
                } else {
                    if eof && !pending.is_empty() {
                        text.push('\u{fffd}');
                        pending.clear();
                    }
                    break;
                }
            }
        }
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn utf8_terminal_reads_preserve_split_chinese_characters() {
        let mut pending = Vec::new();
        let mut decoded = String::new();
        for byte in "A中文\x1b[0m\r\n".as_bytes() {
            pending.push(*byte);
            decoded.push_str(&decode_utf8(&mut pending, false));
        }
        assert_eq!(decoded, "A中文\x1b[0m\r\n");
        assert!(pending.is_empty());
    }
}
