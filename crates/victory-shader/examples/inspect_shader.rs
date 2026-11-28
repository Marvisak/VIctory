//! Print GFX10 instructions around one byte offset in a captured DXBC file.
use victory_shader::{Container, FourCC, gfx10, ir, native_programs, structure};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1);
    let path = args.next().ok_or("usage: inspect_shader FILE OFFSET|all")?;
    let at = args.next().ok_or("missing offset")?;
    let offset = if at == "all" {
        usize::MAX
    } else {
        usize::from_str_radix(at.trim_start_matches("0x"), 16)?
    };
    let bytes = std::fs::read(path)?;
    let container = Container::parse(&bytes)?;
    let programs = native_programs(container.part(FourCC::DXIL).ok_or("no DXIL part")?.data)?;
    for program in programs {
        println!(
            "{:?} io {:?} root layout {:?} used {:?}",
            program.stage, program.io, program.layout, program.used
        );
        for inst in gfx10::decode(&program.code)? {
            if offset == usize::MAX || inst.offset.abs_diff(offset) <= 48 {
                println!("{:#06x}: {}", inst.offset, inst.text(program.wave64));
                if inst.offset == offset {
                    println!("        {:?}", inst.fields(program.wave64));
                }
            }
        }
        let p = ir::build(&program.code, program.wave64, program.vgprs)?;
        let st = structure::structure(&p)?;
        for (b, block) in p.blocks.iter().enumerate() {
            for inst in &block.insts {
                if offset == usize::MAX || inst.machine.offset.abs_diff(offset) <= 48 {
                    println!("IR block {b} {:#x} {} reads={:?} writes={:?}", inst.machine.offset, inst.name(), inst.reads, inst.writes);
                    for &(_, v) in &inst.writes {
                        if let Some(mask) = st.masks.get(&v) {
                            println!("  structured mask {v:?}={mask:?}");
                        }
                    }
                }
            }
        }
    }
    Ok(())
}
