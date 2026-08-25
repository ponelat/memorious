# Third-party notices

Every Memorious binary (server, CLI, desktop app, and the iOS app built from
`crates/mobile`) statically links the components below via the Rust crate
`rusqlite` (feature `bundled-sqlcipher-vendored-openssl`). Their license terms
apply to those binaries in addition to Memorious's own MIT/Apache-2.0 licensing;
this file is distributed alongside the binaries to satisfy the attribution
conditions. Nothing here implies any endorsement of Memorious by these projects.

## SQLCipher (community edition) — Zetetic LLC

Memorious's encrypted event log is an SQLCipher database.

```
Copyright (c) 2008-2020 Zetetic LLC
All rights reserved.

Redistribution and use in source and binary forms, with or without
modification, are permitted provided that the following conditions are met:
    * Redistributions of source code must retain the above copyright
      notice, this list of conditions and the following disclaimer.
    * Redistributions in binary form must reproduce the above copyright
      notice, this list of conditions and the following disclaimer in the
      documentation and/or other materials provided with the distribution.
    * Neither the name of the ZETETIC LLC nor the
      names of its contributors may be used to endorse or promote products
      derived from this software without specific prior written permission.

THIS SOFTWARE IS PROVIDED BY ZETETIC LLC ''AS IS'' AND ANY
EXPRESS OR IMPLIED WARRANTIES, INCLUDING, BUT NOT LIMITED TO, THE IMPLIED
WARRANTIES OF MERCHANTABILITY AND FITNESS FOR A PARTICULAR PURPOSE ARE
DISCLAIMED. IN NO EVENT SHALL ZETETIC LLC BE LIABLE FOR ANY
DIRECT, INDIRECT, INCIDENTAL, SPECIAL, EXEMPLARY, OR CONSEQUENTIAL DAMAGES
(INCLUDING, BUT NOT LIMITED TO, PROCUREMENT OF SUBSTITUTE GOODS OR SERVICES;
LOSS OF USE, DATA, OR PROFITS; OR BUSINESS INTERRUPTION) HOWEVER CAUSED AND
ON ANY THEORY OF LIABILITY, WHETHER IN CONTRACT, STRICT LIABILITY, OR TORT
(INCLUDING NEGLIGENCE OR OTHERWISE) ARISING IN ANY WAY OUT OF THE USE OF THIS
SOFTWARE, EVEN IF ADVISED OF THE POSSIBILITY OF SUCH DAMAGE.
```

## OpenSSL

SQLCipher's crypto provider in these builds is a vendored OpenSSL 3.x
(`openssl-src`). Copyright The OpenSSL Project Authors. All Rights Reserved.
OpenSSL 3.x is licensed under the Apache License, Version 2.0 — the full text
is included in this repository as [LICENSE-APACHE](LICENSE-APACHE).

## SQLite

SQLCipher is derived from SQLite. SQLite is in the public domain
(https://sqlite.org/copyright.html) and imposes no conditions.
