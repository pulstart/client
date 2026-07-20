use st_protocol::ControlMessage;

pub const DEFAULT_SERVER_PORT: u16 = 28_480;

pub fn normalize_server_address(value: &str) -> Result<String, String> {
    let value = value.trim();
    if value.is_empty() {
        return Err("server address is empty".into());
    }

    if value.parse::<std::net::SocketAddr>().is_ok() {
        return Ok(value.to_string());
    }
    if let Some(host) = value
        .strip_prefix('[')
        .and_then(|value| value.strip_suffix(']'))
    {
        return Ok(format!("[{host}]:{DEFAULT_SERVER_PORT}"));
    }
    if value.parse::<std::net::Ipv6Addr>().is_ok() {
        return Ok(format!("[{value}]:{DEFAULT_SERVER_PORT}"));
    }
    if let Some((host, port)) = value.rsplit_once(':') {
        if !host.is_empty() && !host.contains(':') && port.parse::<u16>().is_ok() {
            return Ok(value.to_string());
        }
        return Err(format!("invalid server address: {value}"));
    }
    Ok(format!("{value}:{DEFAULT_SERVER_PORT}"))
}

pub fn drain_control_messages(buf: &mut Vec<u8>) -> Vec<ControlMessage> {
    let mut messages = Vec::new();
    let mut consumed = 0usize;

    while consumed < buf.len() {
        let remaining = &buf[consumed..];
        if remaining.len() < 3 {
            break;
        }
        let payload_len = u16::from_be_bytes([remaining[1], remaining[2]]) as usize;
        let frame_len = 3 + payload_len;
        if remaining.len() < frame_len {
            break;
        }
        match ControlMessage::deserialize(remaining) {
            Some((message, used)) => {
                messages.push(message);
                consumed += used;
            }
            None => {
                eprintln!(
                    "[client-core] dropping invalid control message type={} payload_len={payload_len}",
                    remaining[0]
                );
                consumed += frame_len;
            }
        }
    }

    if consumed > 0 {
        buf.drain(..consumed);
    }
    messages
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn adds_default_port() {
        assert_eq!(
            normalize_server_address("192.168.1.10").unwrap(),
            "192.168.1.10:28480"
        );
        assert_eq!(
            normalize_server_address("192.168.1.10:1234").unwrap(),
            "192.168.1.10:1234"
        );
        assert_eq!(
            normalize_server_address("stream-box.local:1234").unwrap(),
            "stream-box.local:1234"
        );
        assert_eq!(
            normalize_server_address("2001:db8::1").unwrap(),
            "[2001:db8::1]:28480"
        );
        assert_eq!(
            normalize_server_address("[2001:db8::1]").unwrap(),
            "[2001:db8::1]:28480"
        );
    }

    #[test]
    fn drains_fragmented_and_coalesced_messages() {
        let first = ControlMessage::AuthResult(true).serialize();
        let second = ControlMessage::StreamStarted.serialize();
        let mut pending = first[..2].to_vec();
        assert!(drain_control_messages(&mut pending).is_empty());

        pending.extend_from_slice(&first[2..]);
        pending.extend_from_slice(&second);
        assert_eq!(
            drain_control_messages(&mut pending),
            vec![
                ControlMessage::AuthResult(true),
                ControlMessage::StreamStarted
            ]
        );
        assert!(pending.is_empty());
    }

    #[test]
    fn skips_complete_invalid_message() {
        let valid = ControlMessage::StreamStarted.serialize();
        let mut pending = vec![0xff, 0, 0];
        pending.extend_from_slice(&valid);
        assert_eq!(
            drain_control_messages(&mut pending),
            vec![ControlMessage::StreamStarted]
        );
        assert!(pending.is_empty());
    }
}
