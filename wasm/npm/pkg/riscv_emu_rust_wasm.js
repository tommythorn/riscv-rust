import * as wasm from "./riscv_emu_rust_wasm_bg.wasm";
import { __wbg_set_wasm } from "./riscv_emu_rust_wasm_bg.js";
__wbg_set_wasm(wasm);
export * from "./riscv_emu_rust_wasm_bg.js";
