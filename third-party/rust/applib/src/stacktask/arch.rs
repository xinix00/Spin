//! Alleen registertransport; geen Rust-lening of destructor leeft in deze assembly.
use super::Registers;
use core::arch::naked_asm;
#[cfg(all(target_arch = "aarch64", not(target_abi = "softfloat")))]
macro_rules! arm_save_float {()=>{"stp d8,d9,[x0,#112]\nstp d10,d11,[x0,#128]\nstp d12,d13,[x0,#144]\nstp d14,d15,[x0,#160]\nmrs x9,fpcr\nstr x9,[x0,#208]\nmrs x9,fpsr\nstr x9,[x0,#216]"};}
#[cfg(all(target_arch = "aarch64", target_abi = "softfloat"))]
macro_rules! arm_save_float {
    () => {
        ""
    };
}
#[cfg(all(target_arch = "aarch64", not(target_abi = "softfloat")))]
macro_rules! arm_load_float {()=>{"ldp d8,d9,[x1,#112]\nldp d10,d11,[x1,#128]\nldp d12,d13,[x1,#144]\nldp d14,d15,[x1,#160]\nldr x9,[x1,#208]\nmsr fpcr,x9\nldr x9,[x1,#216]\nmsr fpsr,x9"};}
#[cfg(all(target_arch = "aarch64", target_abi = "softfloat"))]
macro_rules! arm_load_float {
    () => {
        ""
    };
}
/// # Safety
/// Geldige, verschillende contexten op deze core; beide stacks blijven gereserveerd.
#[cfg(target_arch = "aarch64")]
#[unsafe(naked)]
pub(super) unsafe extern "C" fn switch(_from: *mut Registers, _to: *const Registers) {
    naked_asm!(
        "stp x19,x20,[x0,#0]",
        "stp x21,x22,[x0,#16]",
        "stp x23,x24,[x0,#32]",
        "stp x25,x26,[x0,#48]",
        "stp x27,x28,[x0,#64]",
        "stp x29,x30,[x0,#80]",
        "mov x9,sp",
        "str x9,[x0,#96]",
        arm_save_float!(),
        "ldp x19,x20,[x1,#0]",
        "ldp x21,x22,[x1,#16]",
        "ldp x23,x24,[x1,#32]",
        "ldp x25,x26,[x1,#48]",
        "ldp x27,x28,[x1,#64]",
        "ldp x29,x30,[x1,#80]",
        "ldr x9,[x1,#96]",
        "mov sp,x9",
        arm_load_float!(),
        "ret",
    );
}
#[cfg(target_arch = "aarch64")]
#[unsafe(naked)]
unsafe extern "C" fn start() -> ! {
    naked_asm!("mov x0,x19", "blr x20", "brk #0");
}
/// # Safety
/// Zelfde contract als switch; de nieuwe stack is leeg, schrijfbaar en aligned.
#[cfg(target_arch = "aarch64")]
pub(super) unsafe fn init(
    r: &mut Registers,
    top: usize,
    state: *mut (),
    entry: extern "C" fn(*mut ()) -> !,
) {
    r.general[0] = state as u64;
    r.general[1] = entry as usize as u64;
    r.general[11] = start as *const () as usize as u64;
    r.general[12] = top as u64;
}

// De ondersteunde RISC-V-SDK gebruikt lp64d. Rust 1.93 exposeert F/D niet
// in cfg(target_feature); de assembler moet ze hier expliciet krijgen.
#[cfg(target_arch = "riscv64")]
macro_rules! rv_save_float {()=>{".option push\n.option arch,+d\nfsd fs0,112(a0)\nfsd fs1,120(a0)\nfsd fs2,128(a0)\nfsd fs3,136(a0)\nfsd fs4,144(a0)\nfsd fs5,152(a0)\nfsd fs6,160(a0)\nfsd fs7,168(a0)\nfsd fs8,176(a0)\nfsd fs9,184(a0)\nfsd fs10,192(a0)\nfsd fs11,200(a0)\nfrcsr t0\nsd t0,208(a0)\n.option pop"};}
#[cfg(target_arch = "riscv64")]
macro_rules! rv_load_float {()=>{".option push\n.option arch,+d\nfld fs0,112(a1)\nfld fs1,120(a1)\nfld fs2,128(a1)\nfld fs3,136(a1)\nfld fs4,144(a1)\nfld fs5,152(a1)\nfld fs6,160(a1)\nfld fs7,168(a1)\nfld fs8,176(a1)\nfld fs9,184(a1)\nfld fs10,192(a1)\nfld fs11,200(a1)\nld t0,208(a1)\nfscsr t0\n.option pop"};}
#[cfg(target_arch = "riscv64")]
#[unsafe(naked)]
pub(super) unsafe extern "C" fn switch(_from: *mut Registers, _to: *const Registers) {
    naked_asm!(
        "sd s0,0(a0)",
        "sd s1,8(a0)",
        "sd s2,16(a0)",
        "sd s3,24(a0)",
        "sd s4,32(a0)",
        "sd s5,40(a0)",
        "sd s6,48(a0)",
        "sd s7,56(a0)",
        "sd s8,64(a0)",
        "sd s9,72(a0)",
        "sd s10,80(a0)",
        "sd s11,88(a0)",
        "sd sp,96(a0)",
        "sd ra,104(a0)",
        rv_save_float!(),
        "ld s0,0(a1)",
        "ld s1,8(a1)",
        "ld s2,16(a1)",
        "ld s3,24(a1)",
        "ld s4,32(a1)",
        "ld s5,40(a1)",
        "ld s6,48(a1)",
        "ld s7,56(a1)",
        "ld s8,64(a1)",
        "ld s9,72(a1)",
        "ld s10,80(a1)",
        "ld s11,88(a1)",
        "ld sp,96(a1)",
        "ld ra,104(a1)",
        rv_load_float!(),
        "ret",
    );
}
#[cfg(target_arch = "riscv64")]
#[unsafe(naked)]
unsafe extern "C" fn start() -> ! {
    naked_asm!("mv a0,s0", "jalr s1", "unimp");
}
#[cfg(target_arch = "riscv64")]
pub(super) unsafe fn init(
    r: &mut Registers,
    top: usize,
    state: *mut (),
    entry: extern "C" fn(*mut ()) -> !,
) {
    r.general[0] = state as u64;
    r.general[1] = entry as usize as u64;
    r.general[12] = top as u64;
    r.general[13] = start as *const () as usize as u64;
}

#[cfg(target_arch = "x86_64")]
#[unsafe(naked)]
pub(super) unsafe extern "sysv64" fn switch(_from: *mut Registers, _to: *const Registers) {
    naked_asm!(
        "mov [rdi],rbx",
        "mov [rdi+8],rbp",
        "mov [rdi+16],r12",
        "mov [rdi+24],r13",
        "mov [rdi+32],r14",
        "mov [rdi+40],r15",
        "mov [rdi+48],rsp",
        "stmxcsr [rdi+208]",
        "fnstcw [rdi+216]",
        "mov rbx,[rsi]",
        "mov rbp,[rsi+8]",
        "mov r12,[rsi+16]",
        "mov r13,[rsi+24]",
        "mov r14,[rsi+32]",
        "mov r15,[rsi+40]",
        "mov rsp,[rsi+48]",
        "ldmxcsr [rsi+208]",
        "fldcw [rsi+216]",
        "ret"
    );
}
#[cfg(target_arch = "x86_64")]
#[unsafe(naked)]
unsafe extern "C" fn start() -> ! {
    naked_asm!("mov rdi,r12", "sub rsp,8", "call r13", "ud2");
}
#[cfg(target_arch = "x86_64")]
pub(super) unsafe fn init(
    r: &mut Registers,
    top: usize,
    state: *mut (),
    entry: extern "C" fn(*mut ()) -> !,
) {
    r.general[2] = state as u64;
    r.general[3] = entry as usize as u64;
    r.general[6] = (top - 8) as u64;
    r.control = [0x1f80, 0x37f];
    // SAFETY: De constructor levert een lege, 16-byte aligned stack met minimaal 64 KiB.
    unsafe {
        ((top - 8) as *mut usize).write(start as *const () as usize);
    }
}
