
use std::collections::VecDeque;
use std::io;

use msgs::codec;
use msgs::codec::Codec;
use msgs::message::Message;

const HEADER_SIZE: usize = 1 + 2 + 2;

/// This is the maximum on-the-wire size of a TLSCiphertext.
/// That's 2^14 payload bytes, a header, and a 2KB allowance
/// for ciphertext overheads.
const MAX_MESSAGE: usize = 16384 + 2048 + HEADER_SIZE;

/// Bound on our unprocessed frames queue. Arbitrarily chosen.
const QUEUE_SIZE: usize = 1024;

/// This deframer works to reconstruct TLS messages
/// from arbitrary-sized reads, buffering as neccessary.
/// The input is `read()`, the output is the `frames` deque.
pub struct MessageDeframer {
    /// Completed frames for output.
    pub frames: VecDeque<Message>,

    /// Set to true if the peer is not talking TLS, but some other
    /// protocol.  The caller should abort the connection, because
    /// the deframer cannot recover.
    pub desynced: bool,

    /// A variable-size buffer containing the currently-
    /// accumulating TLS message.
    buf: Vec<u8>,
}

impl MessageDeframer {
    pub fn new() -> MessageDeframer {
        MessageDeframer {
            frames: VecDeque::new(),
            desynced: false,
            buf: Vec::with_capacity(MAX_MESSAGE),
        }
    }

    /// Read some bytes from `rd`, and add them to our internal
    /// buffer.  If this means our internal buffer contains
    /// full messages, decode them all.
    pub fn read(&mut self, rd: &mut io::Read) -> io::Result<usize> {
        if self.frames.len() > QUEUE_SIZE { return Ok(0) }

        // Try to do the largest reads possible.  Note that if
        // we get a message with a length field out of range here,
        // we do a zero length read.  That looks like an EOF to
        // the next layer up, which is fine.
        let used = self.buf.len();
        self.buf.resize(MAX_MESSAGE, 0u8);
        let rc = rd.read(&mut self.buf[used..MAX_MESSAGE]);

        if rc.is_err() {
            // Discard indeterminate bytes.
            self.buf.truncate(used);
            return rc;
        }

        let new_bytes = rc.unwrap();
        self.buf.truncate(used + new_bytes);

        loop {
            match self.buf_contains_message() {
                None => {
                    self.desynced = true;
                    break;
                }
                Some(true) => {
                    self.deframe_one();
                }
                Some(false) => break,
            }
        }

        Ok(new_bytes)
    }

    /// Returns true if we have messages for the caller
    /// to process, either whole messages in our output
    /// queue or partial messages in our buffer.
    pub fn has_pending(&self) -> bool {
        !self.frames.is_empty() || !self.buf.is_empty()
    }

    /// Does our `buf` contain a full message?  It does if it is big enough to
    /// contain a header, and that header has a length which falls within `buf`.
    /// This returns None if it contains a header which is invalid.
    fn buf_contains_message(&self) -> Option<bool> {
        if self.buf.len() < HEADER_SIZE {
            return Some(false);
        }

        let len_maybe = Message::check_header(&self.buf);

        // Header damaged.
        if len_maybe == None {
            return None;
        }

        let len = len_maybe.unwrap();

        // This is just too large.
        if len >= MAX_MESSAGE - HEADER_SIZE {
            return None;
        }

        let full_message = self.buf.len() >= len + HEADER_SIZE;
        Some(full_message)
    }

    /// Take a TLS message off the front of `buf`, and put it onto the back
    /// of our `frames` deque.
    fn deframe_one(&mut self) {
        let used = {
            let mut rd = codec::Reader::init(&self.buf);
            let m = Message::read(&mut rd).unwrap();
            self.frames.push_back(m);
            rd.used()
        };
        self.buf = self.buf.split_off(used);
    }
}

#[cfg(test)]
mod tests {
    use super::MessageDeframer;
    use std::io;
    use msgs;

    const FIRST_MESSAGE: &'static [u8] = include_bytes!("deframer-test.1.bin");
    const SECOND_MESSAGE: &'static [u8] = include_bytes!("deframer-test.2.bin");

    struct ByteRead<'a> {
        buf: &'a [u8],
        offs: usize,
    }

    impl<'a> ByteRead<'a> {
        fn new(bytes: &'a [u8]) -> ByteRead {
            ByteRead {
                buf: bytes,
                offs: 0,
            }
        }
    }

    impl<'a> io::Read for ByteRead<'a> {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            let mut len = 0;

            while len < buf.len() && len < self.buf.len() - self.offs {
                buf[len] = self.buf[self.offs + len];
                len += 1;
            }

            self.offs += len;

            Ok(len)
        }
    }

    fn input_bytes(d: &mut MessageDeframer, bytes: &[u8]) -> io::Result<usize> {
        let mut rd = ByteRead::new(bytes);
        d.read(&mut rd)
    }

    fn input_whole_incremental(d: &mut MessageDeframer, bytes: &[u8]) {
        let frames_before = d.frames.len();

        for i in 0..bytes.len() {
            assert_len(1, input_bytes(d, &bytes[i..i + 1]));
            assert_eq!(d.has_pending(), true);

            if i < bytes.len() - 1 {
                assert_eq!(frames_before, d.frames.len());
            }
        }

        assert_eq!(frames_before + 1, d.frames.len());
    }

    fn assert_len(want: usize, got: io::Result<usize>) {
        if let Ok(gotval) = got {
            assert_eq!(gotval, want);
        } else {
            assert!(false, "read failed, expected {:?} bytes", want);
        }
    }

    fn pop_first(d: &mut MessageDeframer) {
        let mut m = d.frames.pop_front().unwrap();
        m.decode_payload();
        assert_eq!(m.typ, msgs::enums::ContentType::Handshake);
    }

    fn pop_second(d: &mut MessageDeframer) {
        let mut m = d.frames.pop_front().unwrap();
        m.decode_payload();
        assert_eq!(m.typ, msgs::enums::ContentType::Alert);
    }

    #[test]
    fn check_incremental() {
        let mut d = MessageDeframer::new();
        assert_eq!(d.has_pending(), false);
        input_whole_incremental(&mut d, FIRST_MESSAGE);
        assert_eq!(d.has_pending(), true);
        assert_eq!(1, d.frames.len());
        pop_first(&mut d);
        assert_eq!(d.has_pending(), false);
    }

    #[test]
    fn check_incremental_2() {
        let mut d = MessageDeframer::new();
        assert_eq!(d.has_pending(), false);
        input_whole_incremental(&mut d, FIRST_MESSAGE);
        assert_eq!(d.has_pending(), true);
        input_whole_incremental(&mut d, SECOND_MESSAGE);
        assert_eq!(d.has_pending(), true);
        assert_eq!(2, d.frames.len());
        pop_first(&mut d);
        assert_eq!(d.has_pending(), true);
        pop_second(&mut d);
        assert_eq!(d.has_pending(), false);
    }

    #[test]
    fn check_whole() {
        let mut d = MessageDeframer::new();
        assert_eq!(d.has_pending(), false);
        assert_len(FIRST_MESSAGE.len(), input_bytes(&mut d, FIRST_MESSAGE));
        assert_eq!(d.has_pending(), true);
        assert_eq!(d.frames.len(), 1);
        pop_first(&mut d);
        assert_eq!(d.has_pending(), false);
    }

    #[test]
    fn check_whole_2() {
        let mut d = MessageDeframer::new();
        assert_eq!(d.has_pending(), false);
        assert_len(FIRST_MESSAGE.len(), input_bytes(&mut d, FIRST_MESSAGE));
        assert_len(SECOND_MESSAGE.len(), input_bytes(&mut d, SECOND_MESSAGE));
        assert_eq!(d.frames.len(), 2);
        pop_first(&mut d);
        pop_second(&mut d);
        assert_eq!(d.has_pending(), false);
    }
}
