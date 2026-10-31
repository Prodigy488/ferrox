//! A whole-machine smoke test: RAM + an MMIO device + a port device, driven
//! by the reference CPU running a small hand-assembled program.

use ferrox_core::Bus;
use ferrox_cpu::{CpuBackend, Exit, RefCpu};
use ferrox_machine::{map, Mmio, PhysMem, PortDevice};
use std::sync::{Arc, Mutex};

struct Recorder(Arc<Mutex<Vec<u64>>>);
impl Mmio for Recorder {
    fn read(&mut self, _o: u64, _s: usize) -> u64 {
        0
    }
    fn write(&mut self, _o: u64, _s: usize, v: u64) {
        self.0.lock().unwrap().push(v);
    }
}

struct Port80(Arc<Mutex<Vec<u64>>>);
impl PortDevice for Port80 {
    fn io_in(&mut self, _p: u16, _s: usize) -> u64 {
        0
    }
    fn io_out(&mut self, _p: u16, _s: usize, v: u64) {
        self.0.lock().unwrap().push(v);
    }
}

#[test]
fn counts_to_five_into_a_device_and_posts_a_code() {
    let writes = Arc::new(Mutex::new(Vec::new()));
    let posts = Arc::new(Mutex::new(Vec::new()));
    let mut m = PhysMem::new();
    m.add_ram(map::RAM_BASE, 1 << 20).unwrap();
    m.add_mmio(map::NV2A_BASE, map::NV2A_SIZE, Box::new(Recorder(writes.clone()))).unwrap();
    m.add_ports(0x80..=0x80, Box::new(Port80(posts.clone())));

    let dev = (map::NV2A_BASE as u32).to_le_bytes();
    #[rustfmt::skip]
    let prog: Vec<u8> = [
        &[0xB9, 5, 0, 0, 0][..],            // mov ecx, 5
        &[0xB8, 0, 0, 0, 0],                // mov eax, 0
        &[0x05, 1, 0, 0, 0],                // loop: add eax, 1
        &[0xA3, dev[0], dev[1], dev[2], dev[3]], //       mov [device], eax
        &[0x49],                            //       dec ecx
        &[0x75, 0xF3],                      //       jne loop
        &[0xB8, 0x42, 0, 0, 0],             // mov eax, 0x42
        &[0xE6, 0x80],                      // out 0x80, al
        &[0xF4],                            // hlt
    ].concat();
    m.load(0x1000, &prog).unwrap();

    let mut cpu = RefCpu::new(0x1000);
    assert_eq!(cpu.run(&mut m, 1000).unwrap(), Exit::Halt);
    assert_eq!(*writes.lock().unwrap(), [1, 2, 3, 4, 5]);
    assert_eq!(*posts.lock().unwrap(), [0x42]);
    assert!(m.read(0x1000, 1).is_ok());
}

#[test]
fn an_unmapped_fetch_is_reported_not_ignored() {
    let mut m = PhysMem::new();
    let mut cpu = RefCpu::new(0x5000_0000);
    assert!(cpu.run(&mut m, 10).is_err());
}

#[test]
fn unknown_opcodes_stop_the_run() {
    let mut m = PhysMem::new();
    m.add_ram(0, 4096).unwrap();
    m.load(0, &[0x0F, 0x0B]).unwrap(); // ud2: not in the test interpreter
    let mut cpu = RefCpu::new(0);
    assert!(matches!(
        cpu.run(&mut m, 10),
        Err(ferrox_cpu::CpuError::Unimplemented { eip: 0, opcode: 0x0F })
    ));
}
