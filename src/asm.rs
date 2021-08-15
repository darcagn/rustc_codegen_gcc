use gccjit::{RValue, ToRValue, Type};
use rustc_ast::ast::{InlineAsmOptions, InlineAsmTemplatePiece};
use rustc_codegen_ssa::mir::operand::OperandValue;
use rustc_codegen_ssa::mir::place::PlaceRef;
use rustc_codegen_ssa::traits::{AsmBuilderMethods, AsmMethods, BaseTypeMethods, BuilderMethods, GlobalAsmOperandRef, InlineAsmOperandRef};
use rustc_data_structures::fx::FxHashMap;
use rustc_hir::LlvmInlineAsmInner;
use rustc_middle::bug;
use rustc_span::Span;
use rustc_target::asm::*;

use crate::builder::Builder;
use crate::context::CodegenCx;
use crate::type_of::LayoutGccExt;

impl<'a, 'gcc, 'tcx> AsmBuilderMethods<'tcx> for Builder<'a, 'gcc, 'tcx> {
    fn codegen_llvm_inline_asm(&mut self, _ia: &LlvmInlineAsmInner, _outputs: Vec<PlaceRef<'tcx, RValue<'gcc>>>, mut _inputs: Vec<RValue<'gcc>>, _span: Span) -> bool {
        // TODO(antoyo)
        return true;
    }

    fn codegen_inline_asm(&mut self, template: &[InlineAsmTemplatePiece], operands: &[InlineAsmOperandRef<'tcx, Self>], options: InlineAsmOptions, _span: &[Span]) {
        let asm_arch = self.tcx.sess.asm_arch.unwrap();

        let intel_dialect =
            match asm_arch {
                InlineAsmArch::X86 | InlineAsmArch::X86_64 if !options.contains(InlineAsmOptions::ATT_SYNTAX) => true,
                _ => false,
            };

        // Collect the types of output operands
        // FIXME(antoyo): we do this here instead of later because of a bug in libgccjit where creating the
        // variable after the extended asm expression causes a segfault:
        // https://gcc.gnu.org/bugzilla/show_bug.cgi?id=100380
        let mut output_vars = FxHashMap::default();
        let mut operand_numbers = FxHashMap::default();
        let mut current_number = 0;
        for (idx, op) in operands.iter().enumerate() {
            match *op {
                InlineAsmOperandRef::Out { place, .. } => {
                    let ty =
                        match place {
                            Some(place) => place.layout.gcc_type(self.cx, false),
                            None => {
                                // If the output is discarded, we don't really care what
                                // type is used. We're just using this to tell GCC to
                                // reserve the register.
                                //dummy_output_type(self.cx, reg.reg_class())

                                // NOTE: if no output value, we should not create one (it will be a
                                // clobber).
                                continue;
                            },
                        };
                    let var = self.current_func().new_local(None, ty, "output_register");
                    operand_numbers.insert(idx, current_number);
                    current_number += 1;
                    output_vars.insert(idx, var);
                }
                InlineAsmOperandRef::InOut { out_place, .. } => {
                    let ty =
                        match out_place {
                            Some(place) => place.layout.gcc_type(self.cx, false),
                            None => {
                                // NOTE: if no output value, we should not create one.
                                continue;
                            },
                        };
                    operand_numbers.insert(idx, current_number);
                    current_number += 1;
                    let var = self.current_func().new_local(None, ty, "output_register");
                    output_vars.insert(idx, var);
                }
                _ => {}
            }
        }

        // All output operands must come before the input operands, hence the 2 loops.
        for (idx, op) in operands.iter().enumerate() {
            match *op {
                InlineAsmOperandRef::In { .. } | InlineAsmOperandRef::InOut { .. } => {
                    operand_numbers.insert(idx, current_number);
                    current_number += 1;
                },
                _ => (),
            }
        }

        // Build the template string
        let mut template_str = String::new();
        for piece in template {
            match *piece {
                InlineAsmTemplatePiece::String(ref string) => {
                    if string.contains('%') {
                        for c in string.chars() {
                            if c == '%' {
                                template_str.push_str("%%");
                            }
                            else {
                                template_str.push(c);
                            }
                        }
                    }
                    else {
                        template_str.push_str(string)
                    }
                }
                InlineAsmTemplatePiece::Placeholder { operand_idx, modifier, span: _ } => {
                    match operands[operand_idx] {
                        InlineAsmOperandRef::Out { reg, place: Some(_), ..  } => {
                            let modifier = modifier_to_gcc(asm_arch, reg.reg_class(), modifier);
                            if let Some(modifier) = modifier {
                                template_str.push_str(&format!("%{}{}", modifier, operand_numbers[&operand_idx]));
                            } else {
                                template_str.push_str(&format!("%{}", operand_numbers[&operand_idx]));
                            }
                        },
                        InlineAsmOperandRef::Out { place: None, .. } => {
                            unimplemented!("Out None");
                        },
                        InlineAsmOperandRef::In { reg, .. }
                        | InlineAsmOperandRef::InOut { reg, .. } => {
                            let modifier = modifier_to_gcc(asm_arch, reg.reg_class(), modifier);
                            if let Some(modifier) = modifier {
                                template_str.push_str(&format!("%{}{}", modifier, operand_numbers[&operand_idx]));
                            } else {
                                template_str.push_str(&format!("%{}", operand_numbers[&operand_idx]));
                            }
                        }
                        InlineAsmOperandRef::Const { ref string } => {
                            // Const operands get injected directly into the template
                            template_str.push_str(string);
                        }
                        InlineAsmOperandRef::SymFn { .. }
                        | InlineAsmOperandRef::SymStatic { .. } => {
                            unimplemented!();
                            // Only emit the raw symbol name
                            //template_str.push_str(&format!("${{{}:c}}", op_idx[&operand_idx]));
                        }
                    }
                }
            }
        }

        let block = self.llbb();
        let template_str =
            if intel_dialect {
                template_str
            }
            else {
                // FIXME(antoyo): this might break the "m" memory constraint:
                // https://stackoverflow.com/a/9347957/389119
                // TODO(antoyo): only set on x86 platforms.
                format!(".att_syntax noprefix\n\t{}\n\t.intel_syntax noprefix", template_str)
            };
        let extended_asm = block.add_extended_asm(None, &template_str);

        // Collect the types of output operands
        let mut output_types = vec![];
        for (idx, op) in operands.iter().enumerate() {
            match *op {
                InlineAsmOperandRef::Out { reg, late, place } => {
                    let ty =
                        match place {
                            Some(place) => place.layout.gcc_type(self.cx, false),
                            None => {
                                // If the output is discarded, we don't really care what
                                // type is used. We're just using this to tell GCC to
                                // reserve the register.
                                dummy_output_type(self.cx, reg.reg_class())
                            },
                        };
                    output_types.push(ty);
                    let prefix = if late { "=" } else { "=&" };
                    let constraint = format!("{}{}", prefix, reg_to_gcc(reg));

                    if place.is_some() {
                        let var = output_vars[&idx];
                        extended_asm.add_output_operand(None, &constraint, var);
                    }
                    else {
                        // NOTE: reg.to_string() returns the register name with quotes around it so
                        // remove them.
                        extended_asm.add_clobber(reg.to_string().trim_matches('"'));
                    }
                }
                InlineAsmOperandRef::InOut { reg, late, in_value, out_place } => {
                    let ty =
                        match out_place {
                            Some(out_place) => out_place.layout.gcc_type(self.cx, false),
                            None => dummy_output_type(self.cx, reg.reg_class())
                        };
                    output_types.push(ty);
                    // TODO(antoyo): prefix of "+" for reading and writing?
                    let prefix = if late { "=" } else { "=&" };
                    let constraint = format!("{}{}", prefix, reg_to_gcc(reg));

                    if out_place.is_some() {
                        let var = output_vars[&idx];
                        // TODO(antoyo): also specify an output operand when out_place is none: that would
                        // be the clobber but clobbers do not support general constraint like reg;
                        // they only support named registers.
                        // Not sure how we can do this. And the LLVM backend does not seem to add a
                        // clobber.
                        extended_asm.add_output_operand(None, &constraint, var);
                    }

                    let constraint = reg_to_gcc(reg);
                    extended_asm.add_input_operand(None, &constraint, in_value.immediate());
                }
                InlineAsmOperandRef::In { reg, value } => {
                    let constraint = reg_to_gcc(reg);
                    extended_asm.add_input_operand(None, &constraint, value.immediate());
                }
                _ => {}
            }
        }

        // Write results to outputs
        for (idx, op) in operands.iter().enumerate() {
            if let InlineAsmOperandRef::Out { place: Some(place), .. }
            | InlineAsmOperandRef::InOut { out_place: Some(place), .. } = *op
            {
                OperandValue::Immediate(output_vars[&idx].to_rvalue()).store(self, place);
            }
        }
    }
}

/// Converts a register class to a GCC constraint code.
// TODO(antoyo): return &'static str instead?
fn reg_to_gcc(reg: InlineAsmRegOrRegClass) -> String {
    match reg {
        // For vector registers LLVM wants the register name to match the type size.
        InlineAsmRegOrRegClass::Reg(reg) => {
            // TODO(antoyo): add support for vector register.
            let constraint =
                match reg.name() {
                    "ax" => "a",
                    "bx" => "b",
                    "cx" => "c",
                    "dx" => "d",
                    "si" => "S",
                    "di" => "D",
                    // TODO(antoyo): for registers like r11, we have to create a register variable: https://stackoverflow.com/a/31774784/389119
                    // TODO(antoyo): in this case though, it's a clobber, so it should work as r11.
                    // Recent nightly supports clobber() syntax, so update to it. It does not seem
                    // like it's implemented yet.
                    name => name, // FIXME(antoyo): probably wrong.
                };
            constraint.to_string()
        },
        InlineAsmRegOrRegClass::RegClass(reg) => match reg {
            InlineAsmRegClass::AArch64(AArch64InlineAsmRegClass::preg) => unimplemented!(),
            InlineAsmRegClass::AArch64(AArch64InlineAsmRegClass::reg) => unimplemented!(),
            InlineAsmRegClass::AArch64(AArch64InlineAsmRegClass::vreg) => unimplemented!(),
            InlineAsmRegClass::AArch64(AArch64InlineAsmRegClass::vreg_low16) => unimplemented!(),
            InlineAsmRegClass::Arm(ArmInlineAsmRegClass::reg) => unimplemented!(),
            InlineAsmRegClass::Arm(ArmInlineAsmRegClass::reg_thumb) => unimplemented!(),
            InlineAsmRegClass::Arm(ArmInlineAsmRegClass::sreg)
            | InlineAsmRegClass::Arm(ArmInlineAsmRegClass::dreg_low16)
            | InlineAsmRegClass::Arm(ArmInlineAsmRegClass::qreg_low8) => unimplemented!(),
            InlineAsmRegClass::Arm(ArmInlineAsmRegClass::sreg_low16)
            | InlineAsmRegClass::Arm(ArmInlineAsmRegClass::dreg_low8)
            | InlineAsmRegClass::Arm(ArmInlineAsmRegClass::qreg_low4) => unimplemented!(),
            InlineAsmRegClass::Arm(ArmInlineAsmRegClass::dreg)
            | InlineAsmRegClass::Arm(ArmInlineAsmRegClass::qreg) => unimplemented!(),
            InlineAsmRegClass::Bpf(_) => unimplemented!(),
            InlineAsmRegClass::Hexagon(HexagonInlineAsmRegClass::reg) => unimplemented!(),
            InlineAsmRegClass::Mips(MipsInlineAsmRegClass::reg) => unimplemented!(),
            InlineAsmRegClass::Mips(MipsInlineAsmRegClass::freg) => unimplemented!(),
            InlineAsmRegClass::Nvptx(NvptxInlineAsmRegClass::reg16) => unimplemented!(),
            InlineAsmRegClass::Nvptx(NvptxInlineAsmRegClass::reg32) => unimplemented!(),
            InlineAsmRegClass::Nvptx(NvptxInlineAsmRegClass::reg64) => unimplemented!(),
            InlineAsmRegClass::PowerPC(PowerPCInlineAsmRegClass::reg) => unimplemented!(),
            InlineAsmRegClass::PowerPC(PowerPCInlineAsmRegClass::reg_nonzero) => unimplemented!(),
            InlineAsmRegClass::PowerPC(PowerPCInlineAsmRegClass::freg) => unimplemented!(),
            InlineAsmRegClass::RiscV(RiscVInlineAsmRegClass::reg) => unimplemented!(),
            InlineAsmRegClass::RiscV(RiscVInlineAsmRegClass::freg) => unimplemented!(),
            InlineAsmRegClass::RiscV(RiscVInlineAsmRegClass::vreg) => unimplemented!(),
            InlineAsmRegClass::X86(X86InlineAsmRegClass::mmx_reg) => unimplemented!(),
            InlineAsmRegClass::X86(X86InlineAsmRegClass::reg) => "r",
            InlineAsmRegClass::X86(X86InlineAsmRegClass::reg_abcd) => unimplemented!(),
            InlineAsmRegClass::X86(X86InlineAsmRegClass::reg_byte) => unimplemented!(),
            InlineAsmRegClass::X86(X86InlineAsmRegClass::xmm_reg)
            | InlineAsmRegClass::X86(X86InlineAsmRegClass::ymm_reg) => unimplemented!(),
            InlineAsmRegClass::X86(X86InlineAsmRegClass::x87_reg) => unimplemented!(),
            InlineAsmRegClass::X86(X86InlineAsmRegClass::zmm_reg) => unimplemented!(),
            InlineAsmRegClass::X86(X86InlineAsmRegClass::kreg) => unimplemented!(),
            InlineAsmRegClass::Wasm(WasmInlineAsmRegClass::local) => unimplemented!(),
            InlineAsmRegClass::SpirV(SpirVInlineAsmRegClass::reg) => {
                bug!("GCC backend does not support SPIR-V")
            }
            InlineAsmRegClass::Err => unreachable!(),
        }
        .to_string(),
    }
}

/// Type to use for outputs that are discarded. It doesn't really matter what
/// the type is, as long as it is valid for the constraint code.
fn dummy_output_type<'gcc, 'tcx>(cx: &CodegenCx<'gcc, 'tcx>, reg: InlineAsmRegClass) -> Type<'gcc> {
    match reg {
        InlineAsmRegClass::AArch64(AArch64InlineAsmRegClass::reg) => cx.type_i32(),
        InlineAsmRegClass::AArch64(AArch64InlineAsmRegClass::preg) => unimplemented!(),
        InlineAsmRegClass::AArch64(AArch64InlineAsmRegClass::vreg)
        | InlineAsmRegClass::AArch64(AArch64InlineAsmRegClass::vreg_low16) => {
            unimplemented!()
        }
        InlineAsmRegClass::Arm(ArmInlineAsmRegClass::reg)
        | InlineAsmRegClass::Arm(ArmInlineAsmRegClass::reg_thumb) => cx.type_i32(),
        InlineAsmRegClass::Arm(ArmInlineAsmRegClass::sreg)
        | InlineAsmRegClass::Arm(ArmInlineAsmRegClass::sreg_low16) => cx.type_f32(),
        InlineAsmRegClass::Arm(ArmInlineAsmRegClass::dreg)
        | InlineAsmRegClass::Arm(ArmInlineAsmRegClass::dreg_low16)
        | InlineAsmRegClass::Arm(ArmInlineAsmRegClass::dreg_low8) => cx.type_f64(),
        InlineAsmRegClass::Arm(ArmInlineAsmRegClass::qreg)
        | InlineAsmRegClass::Arm(ArmInlineAsmRegClass::qreg_low8)
        | InlineAsmRegClass::Arm(ArmInlineAsmRegClass::qreg_low4) => {
            unimplemented!()
        }
        InlineAsmRegClass::Bpf(_) => unimplemented!(),
        InlineAsmRegClass::Hexagon(HexagonInlineAsmRegClass::reg) => cx.type_i32(),
        InlineAsmRegClass::Mips(MipsInlineAsmRegClass::reg) => cx.type_i32(),
        InlineAsmRegClass::Mips(MipsInlineAsmRegClass::freg) => cx.type_f32(),
        InlineAsmRegClass::Nvptx(NvptxInlineAsmRegClass::reg16) => cx.type_i16(),
        InlineAsmRegClass::Nvptx(NvptxInlineAsmRegClass::reg32) => cx.type_i32(),
        InlineAsmRegClass::Nvptx(NvptxInlineAsmRegClass::reg64) => cx.type_i64(),
        InlineAsmRegClass::PowerPC(PowerPCInlineAsmRegClass::reg) => cx.type_i32(),
        InlineAsmRegClass::PowerPC(PowerPCInlineAsmRegClass::reg_nonzero) => cx.type_i32(),
        InlineAsmRegClass::PowerPC(PowerPCInlineAsmRegClass::freg) => cx.type_f64(),
        InlineAsmRegClass::RiscV(RiscVInlineAsmRegClass::reg) => cx.type_i32(),
        InlineAsmRegClass::RiscV(RiscVInlineAsmRegClass::freg) => cx.type_f32(),
        InlineAsmRegClass::RiscV(RiscVInlineAsmRegClass::vreg) => cx.type_f32(),
        InlineAsmRegClass::X86(X86InlineAsmRegClass::reg)
        | InlineAsmRegClass::X86(X86InlineAsmRegClass::reg_abcd) => cx.type_i32(),
        InlineAsmRegClass::X86(X86InlineAsmRegClass::reg_byte) => cx.type_i8(),
        InlineAsmRegClass::X86(X86InlineAsmRegClass::mmx_reg) => unimplemented!(),
        InlineAsmRegClass::X86(X86InlineAsmRegClass::xmm_reg)
        | InlineAsmRegClass::X86(X86InlineAsmRegClass::ymm_reg)
        | InlineAsmRegClass::X86(X86InlineAsmRegClass::zmm_reg) => cx.type_f32(),
        InlineAsmRegClass::X86(X86InlineAsmRegClass::x87_reg) => unimplemented!(),
        InlineAsmRegClass::X86(X86InlineAsmRegClass::kreg) => cx.type_i16(),
        InlineAsmRegClass::Wasm(WasmInlineAsmRegClass::local) => cx.type_i32(),
        InlineAsmRegClass::SpirV(SpirVInlineAsmRegClass::reg) => {
            bug!("LLVM backend does not support SPIR-V")
        },
        InlineAsmRegClass::Err => unreachable!(),
    }
}

impl<'gcc, 'tcx> AsmMethods for CodegenCx<'gcc, 'tcx> {
    fn codegen_global_asm(&self, template: &[InlineAsmTemplatePiece], operands: &[GlobalAsmOperandRef], options: InlineAsmOptions, _line_spans: &[Span]) {
        let asm_arch = self.tcx.sess.asm_arch.unwrap();

        // Default to Intel syntax on x86
        let intel_syntax = matches!(asm_arch, InlineAsmArch::X86 | InlineAsmArch::X86_64)
            && !options.contains(InlineAsmOptions::ATT_SYNTAX);

        // Build the template string
        let mut template_str = String::new();
        for piece in template {
            match *piece {
                InlineAsmTemplatePiece::String(ref string) => {
                    for line in string.lines() {
                        // NOTE: gcc does not allow inline comment, so remove them.
                        let line =
                            if let Some(index) = line.rfind("//") {
                                &line[..index]
                            }
                            else {
                                line
                            };
                        template_str.push_str(line);
                        template_str.push('\n');
                    }
                },
                InlineAsmTemplatePiece::Placeholder { operand_idx, modifier: _, span: _ } => {
                    match operands[operand_idx] {
                        GlobalAsmOperandRef::Const { ref string } => {
                            // Const operands get injected directly into the
                            // template. Note that we don't need to escape $
                            // here unlike normal inline assembly.
                            template_str.push_str(string);
                        }
                    }
                }
            }
        }

        let template_str =
            if intel_syntax {
                format!("{}\n\t.intel_syntax noprefix", template_str)
            }
            else {
                format!(".att_syntax\n\t{}\n\t.intel_syntax noprefix", template_str)
            };
        // NOTE: seems like gcc will put the asm in the wrong section, so set it to .text manually.
        let template_str = format!(".pushsection .text\n{}\n.popsection", template_str);
        self.context.add_top_level_asm(None, &template_str);
    }
}

fn modifier_to_gcc(arch: InlineAsmArch, reg: InlineAsmRegClass, modifier: Option<char>) -> Option<char> {
    match reg {
        InlineAsmRegClass::AArch64(AArch64InlineAsmRegClass::reg) => modifier,
        InlineAsmRegClass::AArch64(AArch64InlineAsmRegClass::preg) => modifier,
        InlineAsmRegClass::AArch64(AArch64InlineAsmRegClass::vreg)
        | InlineAsmRegClass::AArch64(AArch64InlineAsmRegClass::vreg_low16) => {
            unimplemented!()
        }
        InlineAsmRegClass::Arm(ArmInlineAsmRegClass::reg)
        | InlineAsmRegClass::Arm(ArmInlineAsmRegClass::reg_thumb) => unimplemented!(),
        InlineAsmRegClass::Arm(ArmInlineAsmRegClass::sreg)
        | InlineAsmRegClass::Arm(ArmInlineAsmRegClass::sreg_low16) => unimplemented!(),
        InlineAsmRegClass::Arm(ArmInlineAsmRegClass::dreg)
        | InlineAsmRegClass::Arm(ArmInlineAsmRegClass::dreg_low16)
        | InlineAsmRegClass::Arm(ArmInlineAsmRegClass::dreg_low8) => unimplemented!(),
        InlineAsmRegClass::Arm(ArmInlineAsmRegClass::qreg)
        | InlineAsmRegClass::Arm(ArmInlineAsmRegClass::qreg_low8)
        | InlineAsmRegClass::Arm(ArmInlineAsmRegClass::qreg_low4) => {
            unimplemented!()
        }
        InlineAsmRegClass::Bpf(_) => unimplemented!(),
        InlineAsmRegClass::Hexagon(_) => unimplemented!(),
        InlineAsmRegClass::Mips(_) => unimplemented!(),
        InlineAsmRegClass::Nvptx(_) => unimplemented!(),
        InlineAsmRegClass::PowerPC(_) => unimplemented!(),
        InlineAsmRegClass::RiscV(RiscVInlineAsmRegClass::reg)
        | InlineAsmRegClass::RiscV(RiscVInlineAsmRegClass::freg) => unimplemented!(),
        InlineAsmRegClass::RiscV(RiscVInlineAsmRegClass::vreg) => unimplemented!(),
        InlineAsmRegClass::X86(X86InlineAsmRegClass::reg)
        | InlineAsmRegClass::X86(X86InlineAsmRegClass::reg_abcd) => match modifier {
            None if arch == InlineAsmArch::X86_64 => Some('q'),
            None => Some('k'),
            Some('l') => Some('b'),
            Some('h') => Some('h'),
            Some('x') => Some('w'),
            Some('e') => Some('k'),
            Some('r') => Some('q'),
            _ => unreachable!(),
        },
        InlineAsmRegClass::X86(X86InlineAsmRegClass::mmx_reg) => unimplemented!(),
        InlineAsmRegClass::X86(X86InlineAsmRegClass::reg_byte) => unimplemented!(),
        InlineAsmRegClass::X86(X86InlineAsmRegClass::xmm_reg)
        | InlineAsmRegClass::X86(X86InlineAsmRegClass::ymm_reg)
        | InlineAsmRegClass::X86(X86InlineAsmRegClass::zmm_reg) => unimplemented!(),
        InlineAsmRegClass::X86(X86InlineAsmRegClass::x87_reg) => unimplemented!(),
        InlineAsmRegClass::X86(X86InlineAsmRegClass::kreg) => unimplemented!(),
        InlineAsmRegClass::Wasm(WasmInlineAsmRegClass::local) => unimplemented!(),
        InlineAsmRegClass::SpirV(SpirVInlineAsmRegClass::reg) => {
            bug!("LLVM backend does not support SPIR-V")
        },
        InlineAsmRegClass::Err => unreachable!(),
    }
}