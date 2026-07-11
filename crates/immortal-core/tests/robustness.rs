use immortal_core::config::{MAX_CONFIG_BYTES, parse_bytes};
use immortal_core::control::{
    GenerationMatch, Operation, Request, Response, ResponseCode, SignalScope,
};

const CASES: usize = 4_096;
const MAX_MUTATION_BYTES: usize = 4_096;

#[test]
fn deterministic_arbitrary_inputs_never_escape_bounded_decoders() {
    let mut random = XorShift64::new(0x49_4d_4d_4f_52_54_41_4c);
    for _ in 0..CASES {
        let length = random.next_usize(MAX_MUTATION_BYTES + 1);
        let mut bytes = vec![0_u8; length];
        random.fill(&mut bytes);
        drop(parse_bytes(&bytes));
        drop(Request::decode(&bytes));
        drop(Response::decode(&bytes));
    }
}

#[test]
fn every_truncation_and_single_byte_mutation_is_handled() -> Result<(), Box<dyn std::error::Error>>
{
    let request = Request {
        operation: Operation::Status,
        service: "api.worker-1".to_owned(),
        expected_generation: GenerationMatch::Any,
        scope: SignalScope::Main,
        signal: None,
    }
    .encode()?;
    let response = Response {
        code: ResponseCode::Ok,
        generation: None,
        message: "status".to_owned(),
        status: Some(immortal_core::status::StatusSnapshot::from_machine(
            &immortal_core::supervisor::StateMachine::default(),
        )),
    }
    .encode()?;

    for length in 0..request.len() {
        assert!(Request::decode(request.get(..length).ok_or("request slice missing")?).is_err());
    }
    for length in 0..response.len() {
        assert!(Response::decode(response.get(..length).ok_or("response slice missing")?).is_err());
    }
    mutate_each_byte(&request, |bytes| {
        drop(Request::decode(bytes));
    });
    mutate_each_byte(&response, |bytes| {
        drop(Response::decode(bytes));
    });
    Ok(())
}

#[test]
fn oversized_configuration_is_rejected_before_parsing() {
    let bytes = vec![b'a'; MAX_CONFIG_BYTES + 1];
    assert!(parse_bytes(&bytes).is_err());
}

fn mutate_each_byte(input: &[u8], mut decode: impl FnMut(&[u8])) {
    for position in 0..input.len() {
        for mask in [0x01, 0x55, 0x80, 0xff] {
            let mut mutated = input.to_owned();
            if let Some(byte) = mutated.get_mut(position) {
                *byte ^= mask;
            }
            decode(&mutated);
        }
    }
}

struct XorShift64(u64);

impl XorShift64 {
    const fn new(seed: u64) -> Self {
        Self(seed)
    }

    fn next(&mut self) -> u64 {
        let mut value = self.0;
        value ^= value << 13;
        value ^= value >> 7;
        value ^= value << 17;
        self.0 = value;
        value
    }

    fn next_usize(&mut self, upper_bound: usize) -> usize {
        let bound = u64::from(u32::try_from(upper_bound).unwrap_or(u32::MAX));
        usize::try_from(self.next() % bound).unwrap_or(0)
    }

    fn fill(&mut self, bytes: &mut [u8]) {
        for byte in bytes {
            *byte = self.next().to_le_bytes().first().copied().unwrap_or(0);
        }
    }
}
