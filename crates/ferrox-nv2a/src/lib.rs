//! NV2A command stream (pushbuffer) decoding.
//!
//! The GPU is fed through a ring of 32-bit words. Each word either starts a
//! control-flow command (jump, call, return) or a method block: a header
//! naming a subchannel, a starting method address and a word count, followed
//! by that many data words. The first word's bit patterns below match what
//! xemu's `pfifo.c` accepts, which is the reference this was checked against.
//!
//! Decoding is a pure function of the buffer, so it can be unit tested and
//! used for offline tools such as pushbuffer dumpers without any GPU state.

pub mod device;

use std::fmt;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    /// `args.len()` consecutive data words for `method` on `subchannel`.
    /// With `increasing`, word `i` goes to `method + 4 * i`; otherwise every
    /// word goes to `method`.
    Methods { subchannel: u8, method: u32, increasing: bool, args: Vec<u32> },
    /// Jump to a byte offset (new encoding: any 4-byte aligned address).
    Jump(u32),
    /// Jump using the old 29-bit encoding.
    OldJump(u32),
    Call(u32),
    Return,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DecodeError {
    /// A word that is not any known command.
    Reserved { at: u32, word: u32 },
    /// The buffer ends before `at + 4`.
    OutOfBounds { at: u32 },
    /// `call` while already in a subroutine (the hardware has one level).
    NestedCall { at: u32 },
    ReturnWithoutCall { at: u32 },
    /// Too many commands; probably a jump loop.
    StepLimit,
}

impl fmt::Display for DecodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DecodeError::Reserved { at, word } => {
                write!(f, "reserved command word {word:#010x} at {at:#x}")
            }
            DecodeError::OutOfBounds { at } => write!(f, "read past the buffer at {at:#x}"),
            DecodeError::NestedCall { at } => write!(f, "call inside a subroutine at {at:#x}"),
            DecodeError::ReturnWithoutCall { at } => write!(f, "return without call at {at:#x}"),
            DecodeError::StepLimit => write!(f, "command limit reached (jump loop?)"),
        }
    }
}

impl std::error::Error for DecodeError {}

/// Result of decoding up to `put`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Decoded {
    pub commands: Vec<Command>,
    /// Where decoding stopped.
    pub get: u32,
    /// The last method block wanted more data words than the buffer holds
    /// before `put` (the GPU would wait for the CPU to push the rest).
    pub truncated: bool,
}

fn word(buf: &[u8], at: u32) -> Result<u32, DecodeError> {
    let a = at as usize;
    buf.get(a..a + 4)
        .map(|b| u32::from_le_bytes(b.try_into().unwrap()))
        .ok_or(DecodeError::OutOfBounds { at })
}

/// Decodes commands from byte offset `get` until it reaches `put`.
pub fn decode(
    buf: &[u8],
    mut get: u32,
    put: u32,
    max_commands: usize,
) -> Result<Decoded, DecodeError> {
    let mut commands = Vec::new();
    let mut return_to: Option<u32> = None;
    let mut truncated = false;
    while get != put {
        if commands.len() >= max_commands {
            return Err(DecodeError::StepLimit);
        }
        let at = get;
        let w = word(buf, at)?;
        get = get.wrapping_add(4);
        if w & 0xE000_0003 == 0x2000_0000 {
            let target = w & 0x1FFF_FFFF;
            commands.push(Command::OldJump(target));
            get = target;
        } else if w & 3 == 1 {
            let target = w & 0xFFFF_FFFC;
            commands.push(Command::Jump(target));
            get = target;
        } else if w & 3 == 2 {
            if return_to.is_some() {
                return Err(DecodeError::NestedCall { at });
            }
            let target = w & 0xFFFF_FFFC;
            commands.push(Command::Call(target));
            return_to = Some(get);
            get = target;
        } else if w == 0x0002_0000 {
            let Some(back) = return_to.take() else {
                return Err(DecodeError::ReturnWithoutCall { at });
            };
            commands.push(Command::Return);
            get = back;
        } else if w & 0xE003_0003 == 0 || w & 0xE003_0003 == 0x4000_0000 {
            let increasing = w & 0xE003_0003 == 0;
            let count = ((w >> 18) & 0x7FF) as usize;
            let mut args = Vec::with_capacity(count);
            for _ in 0..count {
                if get == put {
                    truncated = true;
                    break;
                }
                args.push(word(buf, get)?);
                get = get.wrapping_add(4);
            }
            commands.push(Command::Methods {
                subchannel: ((w >> 13) & 7) as u8,
                method: w & 0x1FFC,
                increasing,
                args,
            });
            if truncated {
                break;
            }
        } else {
            return Err(DecodeError::Reserved { at, word: w });
        }
    }
    Ok(Decoded { commands, get, truncated })
}

/// One method write: which register, and the value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MethodWrite {
    pub subchannel: u8,
    pub method: u32,
    pub value: u32,
}

/// Flattens method blocks into individual register writes.
pub fn writes(commands: &[Command]) -> impl Iterator<Item = MethodWrite> + '_ {
    commands.iter().flat_map(|c| {
        let (sc, m, inc, args) = match c {
            Command::Methods { subchannel, method, increasing, args } => {
                (*subchannel, *method, *increasing, args.as_slice())
            }
            _ => (0, 0, false, &[][..]),
        };
        args.iter().enumerate().map(move |(i, &v)| MethodWrite {
            subchannel: sc,
            method: if inc { m + 4 * i as u32 } else { m },
            value: v,
        })
    })
}

/// Names for some of the 3D class (NV097) methods; values come from xemu's
/// `nv2a_regs.h`. This is a readability aid for dumps, not a complete table.
pub fn method_name(method: u32) -> Option<&'static str> {
    Some(match method {
        0x0100 => "NO_OPERATION",
        0x0130 => "FLIP_STALL",
        0x0194 => "SET_CONTEXT_DMA_COLOR",
        0x0208 => "SET_SURFACE_FORMAT",
        0x1760..=0x179C => "SET_VERTEX_DATA_ARRAY_FORMAT",
        0x17FC => "SET_BEGIN_END",
        0x1800 => "ARRAY_ELEMENT16",
        0x1810 => "DRAW_ARRAYS",
        0x1818 => "INLINE_ARRAY",
        0x1D94 => "CLEAR_SURFACE",
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bytes(words: &[u32]) -> Vec<u8> {
        words.iter().flat_map(|w| w.to_le_bytes()).collect()
    }

    /// Header for a method block.
    fn header(inc: bool, sc: u32, method: u32, count: u32) -> u32 {
        (if inc { 0 } else { 0x4000_0000 }) | (count << 18) | (sc << 13) | method
    }

    #[test]
    fn increasing_and_non_increasing_blocks() {
        let buf = bytes(&[
            header(true, 0, 0x17FC, 1), 5,
            header(false, 1, 0x1818, 3), 10, 11, 12,
            header(true, 0, 0x1760, 2), 100, 200,
        ]);
        let d = decode(&buf, 0, buf.len() as u32, 100).unwrap();
        assert!(!d.truncated);
        assert_eq!(d.commands.len(), 3);
        let w: Vec<_> = writes(&d.commands).map(|w| (w.subchannel, w.method, w.value)).collect();
        assert_eq!(
            w,
            [
                (0, 0x17FC, 5),
                (1, 0x1818, 10),
                (1, 0x1818, 11),
                (1, 0x1818, 12),
                (0, 0x1760, 100),
                (0, 0x1764, 200),
            ]
        );
    }

    #[test]
    fn calls_return_to_the_word_after_the_call() {
        // 0: call 16        -> runs the subroutine at 16
        // 16: return        -> back to 4
        // 4: method block (1 data word at 8)
        // 12: jump 24       -> end
        let buf = bytes(&[
            16 | 2,
            header(true, 0, 0x130, 1),
            9,
            24 | 1,
            0x0002_0000,
            0,
        ]);
        let d = decode(&buf, 0, 24, 100).unwrap();
        assert_eq!(d.commands.len(), 4);
        assert_eq!(d.commands[0], Command::Call(16));
        assert_eq!(d.commands[1], Command::Return);
        assert!(matches!(d.commands[2], Command::Methods { method: 0x130, .. }));
        assert_eq!(d.commands[3], Command::Jump(24));
        assert_eq!(d.get, 24);
        let w: Vec<_> = writes(&d.commands).collect();
        assert_eq!(w.len(), 1);
        assert_eq!((w[0].method, w[0].value), (0x130, 9));
    }

    #[test]
    fn old_jump_uses_29_bits() {
        let buf = bytes(&[0x2000_0000 | 8, 0, header(true, 0, 0x100, 1), 7]);
        let d = decode(&buf, 0, 16, 10).unwrap();
        assert_eq!(d.commands[0], Command::OldJump(8));
        assert_eq!(writes(&d.commands).count(), 1);
    }

    #[test]
    fn truncated_block_is_reported() {
        let buf = bytes(&[header(true, 0, 0x1760, 4), 1, 2]);
        let d = decode(&buf, 0, buf.len() as u32, 10).unwrap();
        assert!(d.truncated);
        assert_eq!(writes(&d.commands).count(), 2);
    }

    #[test]
    fn errors_are_specific() {
        // 0x0001_0000 matches no form (bit 16 set in a would-be method header)
        let buf = bytes(&[0x0001_0000]);
        assert_eq!(decode(&buf, 0, 4, 10), Err(DecodeError::Reserved { at: 0, word: 0x0001_0000 }));
        let buf = bytes(&[0x0002_0000]);
        assert_eq!(decode(&buf, 0, 4, 10), Err(DecodeError::ReturnWithoutCall { at: 0 }));
        let buf = bytes(&[0 | 2]); // call 0 from 0, then call again
        assert!(matches!(decode(&buf, 0, 8, 10), Err(DecodeError::NestedCall { .. })));
        let buf = bytes(&[1]); // jump to 0: a loop
        assert_eq!(decode(&buf, 0, 4, 50), Err(DecodeError::StepLimit));
        assert_eq!(decode(&[], 0, 4, 10), Err(DecodeError::OutOfBounds { at: 0 }));
    }

    #[test]
    fn names_known_methods() {
        assert_eq!(method_name(0x1D94), Some("CLEAR_SURFACE"));
        assert_eq!(method_name(0x1764), Some("SET_VERTEX_DATA_ARRAY_FORMAT"));
        assert_eq!(method_name(0x9999), None);
    }
}
