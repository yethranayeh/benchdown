# Third-party notices

benchdown is MIT OR Apache-2.0. It contains code and constants adapted from the
projects below.

## EnergiBridge

The RAPL register numbers and unit decoding in `src/meter.rs`, and the AppleSMC
keys in the same file, are ported from EnergiBridge at commit
`a87bd3075484442d0988f230da8c893e78162cbb`, files `src/cpu/intel.rs`,
`src/cpu/amd.rs`, `src/cpu/msr/linux.rs` and `src/cpu/apple.rs`.

EnergiBridge is MIT licensed:

```
MIT License

Copyright (c) 2023 Thomas Durieux

Permission is hereby granted, free of charge, to any person obtaining a copy
of this software and associated documentation files (the "Software"), to deal
in the Software without restriction, including without limitation the rights
to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
copies of the Software, and to permit persons to whom the Software is
furnished to do so, subject to the following conditions:

The above copyright notice and this permission notice shall be included in all
copies or substantial portions of the Software.

THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
SOFTWARE.
```

## smc

On macOS, `src/meter.rs` reads AppleSMC keys through the [`smc`](https://crates.io/crates/smc)
crate, version 0.2.4, by shurizzle (<https://github.com/shurizzle/rust-smc>).
It is used as a dependency and is not vendored. Its licence is the Do What The
Fuck You Want To Public License, version 2:

```
            DO WHAT THE FUCK YOU WANT TO PUBLIC LICENSE
                    Version 2, December 2004

 Copyright (C) 2004 Sam Hocevar <sam@hocevar.net>

 Everyone is permitted to copy and distribute verbatim or modified
 copies of this license document, and changing it is allowed as long
 as the name is changed.

            DO WHAT THE FUCK YOU WANT TO PUBLIC LICENSE
   TERMS AND CONDITIONS FOR COPYING, DISTRIBUTION AND MODIFICATION

  0. You just DO WHAT THE FUCK YOU WANT TO.
```
