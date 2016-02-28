use header::HeaderList;
use header::Header;
use header::ContentLength;
use header::StompHeaderSet;
use header::HeaderCodec;
use std::str::from_utf8;
use std::mem;
use frame::{Frame, Transmission};
use lifeguard::Pool;
use std::collections::VecDeque;

const DEFAULT_STRING_POOL_SIZE: usize = 4;
const DEFAULT_STRING_POOL_MAX_SIZE: usize = 32;
const DEFAULT_HEADER_CODEC_STRING_POOL_SIZE: usize = 16;
const DEFAULT_HEADER_CODEC_STRING_POOL_MAX_SIZE: usize = 64;

pub struct FrameBuffer {
  buffer: VecDeque<u8>,
  parse_state: ParseState,
  string_pool: Pool<String>,
  header_codec: HeaderCodec
}

struct ParseState {
  command: Option<String>,
  headers: HeaderList,
  section: FrameSection,
}

impl ParseState {
  fn new() -> ParseState {
    ParseState {
      command: None,
      headers: header_list![],
      section: FrameSection::Command
    }
  }
}

enum FrameSection {
  Command,
  Headers,
  Body
}

enum ReadCommandResult {
  HeartBeat,
  Command(String),
  Incomplete
}

enum ReadHeaderResult {
  Header(Header),
  EndOfHeaders,
  Incomplete
}

enum ReadBodyResult {
  Body(Vec<u8>),
  Incomplete
}

impl FrameBuffer {
  pub fn new() -> FrameBuffer {
    FrameBuffer::with_capacity(1024 * 64)
  }

  pub fn with_capacity(capacity: usize) -> FrameBuffer {
    FrameBuffer {
      buffer: VecDeque::with_capacity(capacity),
      parse_state: ParseState::new(),
      string_pool: Pool::with_size_and_max(DEFAULT_STRING_POOL_SIZE, DEFAULT_STRING_POOL_MAX_SIZE),
      header_codec: HeaderCodec::with_pool_size_and_max(DEFAULT_HEADER_CODEC_STRING_POOL_SIZE,
                                                        DEFAULT_HEADER_CODEC_STRING_POOL_MAX_SIZE)
    }
  }

  pub fn len(&self) -> usize {
    self.buffer.len()
  }

  pub fn reset(&mut self) {
    self.buffer.clear();
    self.reset_parse_state();
  }

  pub fn append(&mut self, bytes: &[u8]) {
    for byte in bytes {
      self.buffer.push_back(*byte);
    }
    debug!("Copied {} bytes into the frame buffer.", bytes.len());
  }

  pub fn read_transmission(&mut self) -> Option<Transmission> {
    match self.parse_state.section {
      FrameSection::Command => self.resume_parsing_at_command(),
      FrameSection::Headers => self.resume_parsing_at_headers(),
      FrameSection::Body    => self.resume_parsing_at_body()
    }
  }

  fn resume_parsing_at_command(&mut self) -> Option<Transmission> {
    debug!("Parsing command.");
    match self.read_command() {
      ReadCommandResult::HeartBeat => Some(Transmission::HeartBeat),
      ReadCommandResult::Command(command_string) => {
        self.parse_state.command = Some(command_string);
        self.parse_state.section = FrameSection::Headers;
        self.resume_parsing_at_headers()
      },
      ReadCommandResult::Incomplete => None
    }
  }

  fn resume_parsing_at_headers(&mut self) -> Option<Transmission> {
    debug!("Parsing headers.");
    loop {
      match self.read_header() {
        ReadHeaderResult::Header(header) => {
          self.parse_state.headers.push(header);
        },
        ReadHeaderResult::EndOfHeaders => {
          self.parse_state.section = FrameSection::Body;
          return self.resume_parsing_at_body();
        },
        ReadHeaderResult::Incomplete => return None
      }
    }
  }

  fn resume_parsing_at_body(&mut self) -> Option<Transmission> {
    debug!("Parsing body.");
    match self.read_body() {
      ReadBodyResult::Body(body_bytes) => {
        let command = match self.parse_state.command.take() {
          Some(command) => command,
          None => panic!("No COMMAND found.")
        };
        // Consider making the HeaderList an Option<HeaderList> to allow recycling
        let headers = mem::replace(&mut self.parse_state.headers, header_list![]);
        let body = body_bytes;
        let frame = Frame {
          command: command,
          headers: headers,
          body: body
        };
        self.reset_parse_state();
        Some(Transmission::CompleteFrame(frame))
      },
      ReadBodyResult::Incomplete => None
    }
  }

  fn reset_parse_state(&mut self) {
    self.parse_state.section = FrameSection::Command;
  }

  pub fn recycle_frame(&mut self, mut frame: Frame) {
    debug!("Recycling frame.");
    let command: String = frame.command;
    self.string_pool.attach(command);
    frame.headers.drain(|header| self.header_codec.recycle(header));
  }

  // Replace these methods with bridge-buffer concept
  fn read_into_vec(&mut self, n: usize) -> Vec<u8> {
    let mut vec = Vec::with_capacity(n);
    for _ in 0..n {
      let byte = match self.buffer.pop_front() {
        Some(byte) => byte,
        None => panic!("Attempted to read beyond the end of the buffer!")
      };
      vec.push(byte);
    }
    debug!("Removed {} bytes from frame buffer, new size: {}", n, self.buffer.len());
    vec
  }

  fn read_into_string(&mut self, n: usize) -> String {
    let vec = self.read_into_vec(n);
    let s = from_utf8(&vec)
      .ok()
      .expect("Attempted to read a string that was not utf8.");
    self.string_pool.new_from(s).detach()
  }

  fn chomp(mut line: String) -> String {
    // This may be suboptimal, but fine for now
    let chomped_length : usize;
    {
      let chars_to_remove : &[char] = &['\r', '\n'];
      let trimmed_line : &str = line.trim_right_matches(chars_to_remove);
      chomped_length = trimmed_line.len();
    }
    line.truncate(chomped_length);
    line
  }

  fn read_command(&mut self) -> ReadCommandResult {
    use frame_buffer::ReadCommandResult::*;
    match self.find_next('\n' as u8) {
      Some(index) => {
        debug!("Found command ending @ index {}", index);
        let num_bytes = index + 1;
        let command = self.read_into_string(num_bytes as usize);
        let command = FrameBuffer::chomp(command);
        debug!("Chomped length: {}", command.len());
        if command == "" {
          debug!("Found HeartBeat");
          self.string_pool.attach(command);
          return HeartBeat;
        }
        if command.len() == 1 {
          debug!("Byte: {}", command.as_bytes()[0]);
        }
        debug!("Command -> '{}'", command);
        Command(command)
      },
      None => Incomplete
    }
  }

  fn read_header(&mut self) -> ReadHeaderResult {
    match self.find_next('\n' as u8) {
      Some(index) => {
        debug!("Found header ending @ index {}", index);
        let num_bytes = index + 1;
        let header_string = self.read_into_string(num_bytes as usize);
        let header_string = FrameBuffer::chomp(header_string);
        debug!("Header -> '{}'", header_string);
        if header_string == "" {
          self.string_pool.attach(header_string);
          return ReadHeaderResult::EndOfHeaders;
        }
        let header = self.header_codec.decode(&header_string).expect("Invalid header encountered.");
        self.string_pool.attach(header_string);
        ReadHeaderResult::Header(header)
      },
      None => ReadHeaderResult::Incomplete
    }
  }

  fn read_body(&mut self) -> ReadBodyResult {
    let maybe_body : Option<Vec<u8>> = match self.parse_state.headers.get_content_length() {
      Some(ContentLength(num_bytes)) => self.read_body_by_content_length(num_bytes as usize),
      None => self.read_body_by_null_octet()
    };
    match maybe_body {
      Some(body) => ReadBodyResult::Body(body),
      None => ReadBodyResult::Incomplete
    }
  }

  fn read_body_by_content_length(&mut self, content_length: usize) -> Option<Vec<u8>> {
    debug!("Reading body by content length.");
    let bytes_needed = content_length + 1; // null octet
    if self.buffer.len() < bytes_needed {
      debug!("Not enough bytes to form body; needed {}, only had {}.", content_length, self.buffer.len());
      return None;
    }
    let mut body = self.read_into_vec(bytes_needed);
    body.pop(); // Discard null octet
    debug!("Body -> '{}'", FrameBuffer::body_as_string(&body));
    Some(body)
  }

  fn read_body_by_null_octet(&mut self) -> Option<Vec<u8>> {
    debug!("Reading body by null octet.");
    match self.find_next(0u8) {
      Some(index) => {
        debug!("Found body ending @ index {}", index);
        let num_bytes = index + 1;
        let mut body = self.read_into_vec(num_bytes as usize);
        body.pop(); // Discard null octet
        debug!("Body -> '{}'", FrameBuffer::body_as_string(&body));
        Some(body)
      },
      None => None
    }
  }

  fn body_as_string(body: &Vec<u8>) -> &str {
    match from_utf8(&body) {
      Ok(ref s) => *s,
      Err(_) => "<Non-utf8 Binary Content>"
    }
  }

  fn find_next(&self, needle: u8) -> Option<u32> {
    let mut step = 0u32;
    for byte in &self.buffer {
      if *byte == needle {
        return Some(step);
      }
      step += 1;
    }
    None
  }
}
