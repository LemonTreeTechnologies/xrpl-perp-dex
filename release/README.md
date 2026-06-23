# Frozen enclave build environment

`enclave-builder-base.tgz` is a `docker save` of the EXACT toolchain image used
to build the SGX enclave: Ubuntu 22.04 (pinned by @sha256) + apt build tools +
version-pinned Intel SGX SDK 2.28. Freezing the compiler this way makes the
enclave's MRENCLAVE reproducible across machines and across time.

- **sha256:** `0c168459af539384666a1d89333df255f73a8c651820dfdd587308fc712eed99`
- Stored via git-LFS (235 MB).

Reproduce the enclave build with this exact toolchain:

```sh
gunzip -c enclave-builder-base.tgz | docker load        # -> enclave-builder-base:pinned
# then build the enclave FROM enclave-builder-base:pinned
```

To rebuild this file: `docker save` the toolchain layers (everything before the
source COPY) of the enclave repo's `EthSignerEnclave/Dockerfile.azure`.
