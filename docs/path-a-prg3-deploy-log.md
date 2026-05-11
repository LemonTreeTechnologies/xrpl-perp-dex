# PRG-3 Phase A deploy log

Live log of destructive ops bumping Azure DCsv3 cluster from REQ-7.5-era MRENCLAVE `4dfe899771bdb3f3...` to MRENCLAVE_A `e3757b5646ccfc8a055bba0e56e9e3d8e07241346e4ba678b3a8d8fb64fc8fb4` (audit/REQ-8 HEAD `58deb57` + PRG-2 part 1/4 C++ REST endpoints).

Operator-supervised (Andrey at terminal throughout). Sequential per-node (node-1 → node-2 → node-3). Each command + its result captured below verbatim.

## Pre-flight verification (2026-05-11)

Hetzner `/home/andrey/prg3-staging/build-A/` intact:
```
git_sha=58deb57
build_date=2026-05-08T09:43:02Z
enclave_sha256=289760d8586a8b089bdf9a31c8d2d7f48a65f2b63d7827fc7a6644dc10f7aac2
server_sha256=4f1ef108441d92c0ebb17563875c9af015183a2051c149b4dfbb3248d42a0ac5
mrenclave=e3757b5646ccfc8a055bba0e56e9e3d8e07241346e4ba678b3a8d8fb64fc8fb4
```

Azure cluster snapshot:
- sgx-node-1 20.71.184.176: mre=4dfe899771bdb3f3... ver=0.1.0, both services active
- sgx-node-2 20.224.243.60: mre=4dfe899771bdb3f3... ver=0.1.0, both services active
- sgx-node-3 52.236.130.102: mre=4dfe899771bdb3f3... ver=0.1.0, both services active

---

## Phase A — node-1 (20.71.184.176)

(commands + results appended below as they execute)

### A.1 stage artefacts to /tmp/ — 2026-05-11T09:07:07Z

```
$ ssh andrey@94.130.18.162 "scp /home/andrey/prg3-staging/build-A/{enclave.signed.so,perp-dex-server,perp-dex-orchestrator,build-manifest.txt} azureuser@20.71.184.176:/tmp/ 2>&1; ssh azureuser@20.71.184.176 \"sha256sum /tmp/enclave.signed.so /tmp/perp-dex-server /tmp/perp-dex-orchestrator; cat /tmp/build-manifest.txt\""
289760d8586a8b089bdf9a31c8d2d7f48a65f2b63d7827fc7a6644dc10f7aac2  /tmp/enclave.signed.so
4f1ef108441d92c0ebb17563875c9af015183a2051c149b4dfbb3248d42a0ac5  /tmp/perp-dex-server
dfb5ced36965dbf2a441269f67f55b216213d926fbee2bc9d905185ac791ea6a  /tmp/perp-dex-orchestrator
git_sha=58deb57
build_date=2026-05-08T09:43:02Z
image=perp-dex-azure:58deb57
enclave_sha256=289760d8586a8b089bdf9a31c8d2d7f48a65f2b63d7827fc7a6644dc10f7aac2
server_sha256=4f1ef108441d92c0ebb17563875c9af015183a2051c149b4dfbb3248d42a0ac5
mrenclave=e3757b5646ccfc8a055bba0e56e9e3d8e07241346e4ba678b3a8d8fb64fc8fb4
```

### A.2 stop services — 2026-05-11T09:07:42Z

```
$ ssh andrey@94.130.18.162 "ssh azureuser@20.71.184.176 \"sudo systemctl stop perp-dex-orchestrator perp-dex-enclave; systemctl is-active perp-dex-orchestrator perp-dex-enclave; ss -ltn sport = :9088 2>/dev/null | tail -2\""
inactive
inactive
State Recv-Q Send-Q Local Address:Port Peer Address:PortProcess
```

### A.3 backup current artefacts — 2026-05-11T09:08:17Z

```
$ ssh andrey@94.130.18.162 "ssh azureuser@20.71.184.176 \"set -e; cd /home/azureuser/perp; TS=PRG3-20260511-090817; mv enclave.signed.so enclave.signed.so.prg3-pre-\$TS; mv perp-dex-server perp-dex-server.prg3-pre-\$TS; mv perp-dex-orchestrator perp-dex-orchestrator.prg3-pre-\$TS; mv accounts accounts.prg3-pre-\$TS; ls /home/azureuser/perp/ | grep -E \\\"\\.prg3-pre-\$TS\\\" | head; echo ---; ls /home/azureuser/perp/ | grep -vE \\\"prev|pre-|bak|prg3-pre\\\" | head\""
accounts.prg3-pre-
enclave.signed.so.prg3-pre-
perp-dex-orchestrator.prg3-pre-
perp-dex-server.prg3-pre-
---
civetweb_access.log
civetweb_error.log
config.json
deploy.log
enclave.log
orch.log
orchestrator
orchestrator.log
p2p_identity.key
perp.pem
```

### A.3-fix rename empty-suffix backups — 2026-05-11T09:13:45Z

```
drwxr-xr-x  2 azureuser azureuser     4096 May 11 09:04 accounts.prg3-pre-20260511-091345
-rw-r--r--  1 root      root       3213152 Apr 28 15:36 enclave.signed.so.prg3-pre-20260511-091345
-rwxrwxr-x  1 azureuser azureuser 22435632 Apr 28 20:57 perp-dex-orchestrator.prg3-pre-20260511-091345
-rwxr-xr-x  1 root      root       6055016 Apr 28 15:36 perp-dex-server.prg3-pre-20260511-091345
```

### A.4 install new artefacts + fresh accounts/ — 2026-05-11T09:14:22Z

```
drwxr-xr-x  2 azureuser azureuser     4096 May 11 09:14 accounts
-rw-rw-r--  1 azureuser azureuser      311 May 11 09:07 build-manifest.txt
-rw-r--r--  1 azureuser azureuser  3539032 May 11 09:07 enclave.signed.so
-rwxrwxr-x  1 azureuser azureuser 22969640 May 11 09:07 perp-dex-orchestrator
-rwxr-xr-x  1 azureuser azureuser  7957320 May 11 09:07 perp-dex-server
---
289760d8586a8b089bdf9a31c8d2d7f48a65f2b63d7827fc7a6644dc10f7aac2  /home/azureuser/perp/enclave.signed.so
4f1ef108441d92c0ebb17563875c9af015183a2051c149b4dfbb3248d42a0ac5  /home/azureuser/perp/perp-dex-server
```

### A.5 start perp-dex-enclave + verify /version — 2026-05-11T09:14:51Z

```
active
LISTEN 0      200        127.0.0.1:9088      0.0.0.0:*          
---
```

### A.5-retry /version after warmup — 2026-05-11T09:15:25Z

```
--- attempt 1 ---
HTTP 000
--- attempt 2 ---
HTTP 000
--- attempt 3 ---
HTTP 000
--- attempt 4 ---
HTTP 000
--- attempt 5 ---
HTTP 000
---
Server started on port 9088 (HTTPS)
API available at: https://localhost:9088/v1
OpenAPI documentation: https://localhost:9088/docs/openapi.yaml
Version info: https://localhost:9088/version
Error: signal 11:
/home/azureuser/perp/perp-dex-server(+0xe429)[0x5ecd8da61429]
/lib/x86_64-linux-gnu/libc.so.6(+0x42520)[0x7fc6e5842520]
[APP-PATHA-1] DCAP verdict policy: PERP_DCAP_ACCEPTED_QV_RESULTS='OK,SW_HARDENING_NEEDED' → mask=0x3
Using listening port: 127.0.0.1:9088s
Server started on port 9088 (HTTPS)
API available at: https://localhost:9088/v1
OpenAPI documentation: https://localhost:9088/docs/openapi.yaml
Version info: https://localhost:9088/version
Error: signal 11:
/home/azureuser/perp/perp-dex-server(+0xe429)[0x5d8206cb8429]
/lib/x86_64-linux-gnu/libc.so.6(+0x42520)[0x715708042520]
[APP-PATHA-1] DCAP verdict policy: PERP_DCAP_ACCEPTED_QV_RESULTS='OK,SW_HARDENING_NEEDED' → mask=0x3
Using listening port: 127.0.0.1:9088s
Server started on port 9088 (HTTPS)
API available at: https://localhost:9088/v1
OpenAPI documentation: https://localhost:9088/docs/openapi.yaml
Version info: https://localhost:9088/version
Error: signal 11:
/home/azureuser/perp/perp-dex-server(+0xe429)[0x5e25f9174429]
/lib/x86_64-linux-gnu/libc.so.6(+0x42520)[0x7327efa42520]
```

### A.5-incident SIGSEGV crash loop — stop service — 2026-05-11T09:17:19Z

```
inactive
---
May 11 09:14:53 sgx-node-1 systemd[1]: Started Perp DEX Enclave (Azure DCsv3, port 9088).
May 11 09:14:59 sgx-node-1 systemd[1]: perp-dex-enclave.service: Main process exited, code=exited, status=1/FAILURE
May 11 09:14:59 sgx-node-1 systemd[1]: perp-dex-enclave.service: Failed with result 'exit-code'.
May 11 09:15:04 sgx-node-1 systemd[1]: perp-dex-enclave.service: Scheduled restart job, restart counter is at 1.
May 11 09:15:04 sgx-node-1 systemd[1]: Stopped Perp DEX Enclave (Azure DCsv3, port 9088).
May 11 09:15:04 sgx-node-1 systemd[1]: Started Perp DEX Enclave (Azure DCsv3, port 9088).
May 11 09:15:34 sgx-node-1 systemd[1]: perp-dex-enclave.service: Main process exited, code=exited, status=1/FAILURE
May 11 09:15:34 sgx-node-1 systemd[1]: perp-dex-enclave.service: Failed with result 'exit-code'.
May 11 09:15:39 sgx-node-1 systemd[1]: perp-dex-enclave.service: Scheduled restart job, restart counter is at 2.
May 11 09:15:39 sgx-node-1 systemd[1]: Stopped Perp DEX Enclave (Azure DCsv3, port 9088).
May 11 09:15:39 sgx-node-1 systemd[1]: Started Perp DEX Enclave (Azure DCsv3, port 9088).
May 11 09:15:44 sgx-node-1 systemd[1]: perp-dex-enclave.service: Main process exited, code=exited, status=1/FAILURE
May 11 09:15:44 sgx-node-1 systemd[1]: perp-dex-enclave.service: Failed with result 'exit-code'.
May 11 09:15:49 sgx-node-1 systemd[1]: perp-dex-enclave.service: Scheduled restart job, restart counter is at 3.
May 11 09:15:49 sgx-node-1 systemd[1]: Stopped Perp DEX Enclave (Azure DCsv3, port 9088).
May 11 09:15:49 sgx-node-1 systemd[1]: Started Perp DEX Enclave (Azure DCsv3, port 9088).
May 11 09:15:54 sgx-node-1 systemd[1]: perp-dex-enclave.service: Main process exited, code=exited, status=1/FAILURE
May 11 09:15:54 sgx-node-1 systemd[1]: perp-dex-enclave.service: Failed with result 'exit-code'.
May 11 09:15:59 sgx-node-1 systemd[1]: perp-dex-enclave.service: Scheduled restart job, restart counter is at 4.
May 11 09:15:59 sgx-node-1 systemd[1]: Stopped Perp DEX Enclave (Azure DCsv3, port 9088).
May 11 09:15:59 sgx-node-1 systemd[1]: Started Perp DEX Enclave (Azure DCsv3, port 9088).
May 11 09:17:21 sgx-node-1 systemd[1]: Stopping Perp DEX Enclave (Azure DCsv3, port 9088)...
May 11 09:17:21 sgx-node-1 systemd[1]: perp-dex-enclave.service: Deactivated successfully.
May 11 09:17:21 sgx-node-1 systemd[1]: Stopped Perp DEX Enclave (Azure DCsv3, port 9088).
```

### A.5-diag foreground run for full stderr — 2026-05-11T09:25:50Z

```
[APP-PATHA-1] DCAP verdict policy: env unset → enclave default (strict OK only)
Using listening port: 127.0.0.1:9088s
Server started on port 9088 (HTTPS)
API available at: https://localhost:9088/v1
OpenAPI documentation: https://localhost:9088/docs/openapi.yaml
Version info: https://localhost:9088/version
=== exit=124 ===
```

### A.5-diag foreground with systemd env — 2026-05-11T09:26:39Z

```
[APP-PATHA-1] DCAP verdict policy: PERP_DCAP_ACCEPTED_QV_RESULTS='OK,SW_HARDENING_NEEDED' → mask=0x3
Using listening port: 127.0.0.1:9088s
Server started on port 9088 (HTTPS)
API available at: https://localhost:9088/v1
OpenAPI documentation: https://localhost:9088/docs/openapi.yaml
Version info: https://localhost:9088/version
=== exit=124 ===
```

### A.5-diag systemd unit + accounts + binary identity — 2026-05-11T09:27:30Z

```

```

### A.5-diag systemd unit + accounts + binary identity — [Unit]
Description=Perp DEX Enclave (Azure DCsv3, port 9088)
After=network.target aesmd.service
Requires=aesmd.service

[Service]
Type=simple
User=azureuser
WorkingDirectory=/home/azureuser/perp
ExecStart=/home/azureuser/perp/perp-dex-server
Restart=on-failure
RestartSec=5
StandardOutput=append:/home/azureuser/perp/enclave.log
StandardError=append:/home/azureuser/perp/enclave.log

# AZDCAP_DEBUG_LOG_LEVEL is required on Azure: without it, az-dcap-client
# emits "env var not set" to stdout from a subprocess and corrupts the
# DCAP target_info / quote pipeline. See docs/azure_dcap_deployment.md.
Environment=AZDCAP_DEBUG_LOG_LEVEL=FATAL

# APP-PATHA-1: DCAP verdict allowlist for Azure DCsv3 testnet. The
# enclave defaults to strict OK only (mainnet-safe). DCsv3 hosts return
# SW_HARDENING_NEEDED for INTEL-SA-00615 (mitigation applied at the
# hypervisor layer, not visible in the quote). See
# docs/accepted-platform-risks.md P-1 for full rationale. Mainnet (bare
# metal Hetzner) MUST NOT set this — the strict default applies there.
Environment=PERP_DCAP_ACCEPTED_QV_RESULTS=OK,SW_HARDENING_NEEDED

[Install]
WantedBy=multi-user.target
===
total 8
drwxr-xr-x  2 azureuser azureuser 4096 May 11 09:14 .
drwxrwxr-x 12 azureuser azureuser 4096 May 11 09:14 ..
===
-rwxr-xr-x 1 azureuser azureuser 7957320 May 11 09:07 /home/azureuser/perp/perp-dex-server
4f1ef108441d92c0ebb17563875c9af015183a2051c149b4dfbb3248d42a0ac5  /home/azureuser/perp/perp-dex-server

```

```

### A.5-diag 60s foreground for delayed crash — 2026-05-11T09:27:50Z

```
[APP-PATHA-1] DCAP verdict policy: PERP_DCAP_ACCEPTED_QV_RESULTS='OK,SW_HARDENING_NEEDED' → mask=0x3
Using listening port: 127.0.0.1:9088s
Server started on port 9088 (HTTPS)
API available at: https://localhost:9088/v1
OpenAPI documentation: https://localhost:9088/docs/openapi.yaml
Version info: https://localhost:9088/version
=== exit=124 ===
```

### A.5-diag foreground with stdout-to-file (mimics systemd) — 2026-05-11T09:30:26Z

```
=== exit=124 ===
[APP-PATHA-1] DCAP verdict policy: PERP_DCAP_ACCEPTED_QV_RESULTS='OK,SW_HARDENING_NEEDED' → mask=0x3
Using listening port: 127.0.0.1:9088s
Server started on port 9088 (HTTPS)
API available at: https://localhost:9088/v1
OpenAPI documentation: https://localhost:9088/docs/openapi.yaml
Version info: https://localhost:9088/version
```

### A.5-diag re-start systemd + full log triage — 2026-05-11T09:32:11Z

```
=== journalctl pid+exit ===
May 11 09:15:04 sgx-node-1 systemd[1]: Started Perp DEX Enclave (Azure DCsv3, port 9088).
May 11 09:15:34 sgx-node-1 systemd[1]: perp-dex-enclave.service: Main process exited, code=exited, status=1/FAILURE
May 11 09:15:34 sgx-node-1 systemd[1]: perp-dex-enclave.service: Failed with result 'exit-code'.
May 11 09:15:39 sgx-node-1 systemd[1]: perp-dex-enclave.service: Scheduled restart job, restart counter is at 2.
May 11 09:15:39 sgx-node-1 systemd[1]: Stopped Perp DEX Enclave (Azure DCsv3, port 9088).
May 11 09:15:39 sgx-node-1 systemd[1]: Started Perp DEX Enclave (Azure DCsv3, port 9088).
May 11 09:15:44 sgx-node-1 systemd[1]: perp-dex-enclave.service: Main process exited, code=exited, status=1/FAILURE
May 11 09:15:44 sgx-node-1 systemd[1]: perp-dex-enclave.service: Failed with result 'exit-code'.
May 11 09:15:49 sgx-node-1 systemd[1]: perp-dex-enclave.service: Scheduled restart job, restart counter is at 3.
May 11 09:15:49 sgx-node-1 systemd[1]: Stopped Perp DEX Enclave (Azure DCsv3, port 9088).
May 11 09:15:49 sgx-node-1 systemd[1]: Started Perp DEX Enclave (Azure DCsv3, port 9088).
May 11 09:15:54 sgx-node-1 systemd[1]: perp-dex-enclave.service: Main process exited, code=exited, status=1/FAILURE
May 11 09:15:54 sgx-node-1 systemd[1]: perp-dex-enclave.service: Failed with result 'exit-code'.
May 11 09:15:59 sgx-node-1 systemd[1]: perp-dex-enclave.service: Scheduled restart job, restart counter is at 4.
May 11 09:15:59 sgx-node-1 systemd[1]: Stopped Perp DEX Enclave (Azure DCsv3, port 9088).
May 11 09:15:59 sgx-node-1 systemd[1]: Started Perp DEX Enclave (Azure DCsv3, port 9088).
May 11 09:17:21 sgx-node-1 systemd[1]: Stopping Perp DEX Enclave (Azure DCsv3, port 9088)...
May 11 09:17:21 sgx-node-1 systemd[1]: perp-dex-enclave.service: Deactivated successfully.
May 11 09:17:21 sgx-node-1 systemd[1]: Stopped Perp DEX Enclave (Azure DCsv3, port 9088).
May 11 09:32:13 sgx-node-1 systemd[1]: Started Perp DEX Enclave (Azure DCsv3, port 9088).
=== enclave.log tail ===
[APP-PATHA-1] DCAP verdict policy: PERP_DCAP_ACCEPTED_QV_RESULTS='OK,SW_HARDENING_NEEDED' → mask=0x3
Using listening port: 127.0.0.1:9088s
Server started on port 9088 (HTTPS)
API available at: https://localhost:9088/v1
OpenAPI documentation: https://localhost:9088/docs/openapi.yaml
Version info: https://localhost:9088/version
Error: signal 11:
/home/azureuser/perp/perp-dex-server(+0xe429)[0x5d8206cb8429]
/lib/x86_64-linux-gnu/libc.so.6(+0x42520)[0x715708042520]
[APP-PATHA-1] DCAP verdict policy: PERP_DCAP_ACCEPTED_QV_RESULTS='OK,SW_HARDENING_NEEDED' → mask=0x3
Using listening port: 127.0.0.1:9088s
Server started on port 9088 (HTTPS)
API available at: https://localhost:9088/v1
OpenAPI documentation: https://localhost:9088/docs/openapi.yaml
Version info: https://localhost:9088/version
Error: signal 11:
/home/azureuser/perp/perp-dex-server(+0xe429)[0x5e25f9174429]
/lib/x86_64-linux-gnu/libc.so.6(+0x42520)[0x7327efa42520]
[APP-PATHA-1] DCAP verdict policy: PERP_DCAP_ACCEPTED_QV_RESULTS='OK,SW_HARDENING_NEEDED' → mask=0x3
Using listening port: 127.0.0.1:9088s
Server started on port 9088 (HTTPS)
API available at: https://localhost:9088/v1
OpenAPI documentation: https://localhost:9088/docs/openapi.yaml
Version info: https://localhost:9088/version
[APP-PATHA-1] DCAP verdict policy: PERP_DCAP_ACCEPTED_QV_RESULTS='OK,SW_HARDENING_NEEDED' → mask=0x3
Using listening port: 127.0.0.1:9088s
Server started on port 9088 (HTTPS)
API available at: https://localhost:9088/v1
OpenAPI documentation: https://localhost:9088/docs/openapi.yaml
Version info: https://localhost:9088/version
```

### A.5-stable check post-restart — 2026-05-11T09:32:34Z

```
active
LISTEN 0      200        127.0.0.1:9088      0.0.0.0:*          
---
```

### A.5-stable retry curl verbose — 2026-05-11T09:32:46Z

```
azureus+  523468  0.7  0.0 318136 12884 ?        Ssl  09:32   0:00 /home/azureuser/perp/perp-dex-server
---
May 11 09:32:35 sgx-node-1 systemd[1]: perp-dex-enclave.service: Main process exited, code=exited, status=1/FAILURE
May 11 09:32:35 sgx-node-1 systemd[1]: perp-dex-enclave.service: Failed with result 'exit-code'.
May 11 09:32:41 sgx-node-1 systemd[1]: perp-dex-enclave.service: Scheduled restart job, restart counter is at 1.
May 11 09:32:41 sgx-node-1 systemd[1]: Stopped Perp DEX Enclave (Azure DCsv3, port 9088).
May 11 09:32:41 sgx-node-1 systemd[1]: Started Perp DEX Enclave (Azure DCsv3, port 9088).
=== /version retry ===
> Accept: */*
> 
* TLSv1.2 (IN), TLS header, Supplemental data (23):
{ [5 bytes data]
* TLSv1.3 (IN), TLS handshake, Newsession Ticket (4):
{ [249 bytes data]
* OpenSSL SSL_read: Connection reset by peer, errno 104
* Closing connection 0
* TLSv1.2 (OUT), TLS header, Supplemental data (23):
} [5 bytes data]
```

### A.5-diag isolate request triggering crash — 2026-05-11T09:33:33Z

```
active
---
=== curl docs ===
HTTP 000
wc: /tmp/r1.out: No such file or directory
=== sleep 6 ===
active
--- log tail ---
/home/azureuser/perp/perp-dex-server(+0xe429)[0x58b1fcbd9429]
/lib/x86_64-linux-gnu/libc.so.6(+0x42520)[0x72dba1e42520]
[APP-PATHA-1] DCAP verdict policy: PERP_DCAP_ACCEPTED_QV_RESULTS='OK,SW_HARDENING_NEEDED' → mask=0x3
Using listening port: 127.0.0.1:9088s
Server started on port 9088 (HTTPS)
API available at: https://localhost:9088/v1
OpenAPI documentation: https://localhost:9088/docs/openapi.yaml
Version info: https://localhost:9088/version
```

### A.5-diag strace network+signal — 2026-05-11T09:34:55Z

```
=== probe with curl ===
HTTP 000
=== srv stdout ===
[APP-PATHA-1] DCAP verdict policy: PERP_DCAP_ACCEPTED_QV_RESULTS='OK,SW_HARDENING_NEEDED' → mask=0x3
Using listening port: 127.0.0.1:9088s
Failed to start server. Error: Address already in use
Last error code: 98
Checking OpenSSL availability...
OpenSSL library found
=== strace tail signal+last 15 ===
523689 rt_sigaction(SIGSEGV, {sa_handler=0x6276affbe3fa, sa_mask=[SEGV], sa_flags=SA_RESTORER|SA_RESTART, sa_restorer=0x7635c2842520}, {sa_handler=SIG_DFL, sa_mask=[], sa_flags=0}, 8) = 0
523689 rt_sigaction(SIGABRT, {sa_handler=0x6276affbe3fa, sa_mask=[ABRT], sa_flags=SA_RESTORER|SA_RESTART, sa_restorer=0x7635c2842520}, {sa_handler=SIG_DFL, sa_mask=[], sa_flags=0}, 8) = 0
523689 socket(AF_INET, SOCK_STREAM, IPPROTO_TCP) = 3
523689 setsockopt(3, SOL_SOCKET, SO_REUSEADDR, [1], 4) = 0
523689 bind(3, {sa_family=AF_INET, sin_port=htons(9088), sin_addr=inet_addr("127.0.0.1")}, 16) = -1 EADDRINUSE (Address already in use)
523689 +++ exited with 1 +++
```

### A.5-diag strace clean run — 2026-05-11T09:35:27Z

```
=== probe with curl ===
HTTP 000
srv-exit=1
=== srv stdout ===
[APP-PATHA-1] DCAP verdict policy: PERP_DCAP_ACCEPTED_QV_RESULTS='OK,SW_HARDENING_NEEDED' → mask=0x3
Using listening port: 127.0.0.1:9088s
Server started on port 9088 (HTTPS)
API available at: https://localhost:9088/v1
OpenAPI documentation: https://localhost:9088/docs/openapi.yaml
Version info: https://localhost:9088/version
Error: signal 11:
./perp-dex-server(+0xe429)[0x5d278231d429]
/lib/x86_64-linux-gnu/libc.so.6(+0x42520)[0x782b09842520]
=== strace LAST signal events ===
523762 rt_sigprocmask(SIG_SETMASK, [], NULL, 8) = 0
523763 rt_sigprocmask(SIG_SETMASK, [], NULL, 8) = 0
523762 rt_sigprocmask(SIG_BLOCK, ~[],  <unfinished ...>
523763 rt_sigaction(SIGPIPE, {sa_handler=SIG_IGN, sa_mask=[], sa_flags=SA_RESTORER, sa_restorer=0x782b09842520},  <unfinished ...>
523762 rt_sigprocmask(SIG_SETMASK, [],  <unfinished ...>
523764 rt_sigprocmask(SIG_SETMASK, [],  <unfinished ...>
523764 rt_sigaction(SIGPIPE, {sa_handler=SIG_IGN, sa_mask=[], sa_flags=SA_RESTORER, sa_restorer=0x782b09842520}, NULL, 8) = 0
523762 rt_sigprocmask(SIG_BLOCK, ~[], [], 8) = 0
523762 rt_sigprocmask(SIG_SETMASK, [], NULL, 8) = 0
523765 rt_sigprocmask(SIG_SETMASK, [], NULL, 8) = 0
523765 rt_sigaction(SIGPIPE, {sa_handler=SIG_IGN, sa_mask=[], sa_flags=SA_RESTORER, sa_restorer=0x782b09842520},  <unfinished ...>
523762 rt_sigprocmask(SIG_BLOCK, ~[],  <unfinished ...>
523762 rt_sigprocmask(SIG_SETMASK, [], NULL, 8) = 0
523766 rt_sigprocmask(SIG_SETMASK, [], NULL, 8) = 0
523766 rt_sigaction(SIGPIPE, {sa_handler=SIG_IGN, sa_mask=[], sa_flags=SA_RESTORER, sa_restorer=0x782b09842520}, NULL, 8) = 0
523762 rt_sigprocmask(SIG_BLOCK, ~[], [], 8) = 0
523762 rt_sigprocmask(SIG_SETMASK, [],  <unfinished ...>
523767 rt_sigprocmask(SIG_SETMASK, [],  <unfinished ...>
523767 rt_sigaction(SIGPIPE, {sa_handler=SIG_IGN, sa_mask=[], sa_flags=SA_RESTORER, sa_restorer=0x782b09842520}, NULL, 8) = 0
523764 --- SIGSEGV {si_signo=SIGSEGV, si_code=SEGV_MAPERR, si_addr=NULL} ---
```

### A.5-diag foreground+probe — 2026-05-11T09:36:31Z

```
=== srv pid alive? ===
alive
=== probe with curl ===
HTTP 000
=== alive after probe? ===
bash: line 10: kill: (523838) - No such process
DEAD
=== srv stdout ===
[APP-PATHA-1] DCAP verdict policy: PERP_DCAP_ACCEPTED_QV_RESULTS='OK,SW_HARDENING_NEEDED' → mask=0x3
Using listening port: 127.0.0.1:9088s
Server started on port 9088 (HTTPS)
API available at: https://localhost:9088/v1
OpenAPI documentation: https://localhost:9088/docs/openapi.yaml
Version info: https://localhost:9088/version
Error: signal 11:
./perp-dex-server(+0xe429)[0x639b4165d429]
/lib/x86_64-linux-gnu/libc.so.6(+0x42520)[0x79f071242520]
```

### A.5-diag gdb attach — 2026-05-11T09:37:01Z

```
bash: line 1: gdb: command not found
HTTP 000
=== end ===
```

### A.5-diag install gdb — 2026-05-11T09:37:45Z

```
No user sessions are running outdated binaries.

No VM guests are running outdated hypervisor (qemu) binaries on this host.
/usr/bin/gdb
```

### A.5-diag gdb run — 2026-05-11T09:38:06Z

```
=== gdb output ===
[Thread debugging using libthread_db enabled]
Using host libthread_db library "/lib/x86_64-linux-gnu/libthread_db.so.1".
[APP-PATHA-1] DCAP verdict policy: PERP_DCAP_ACCEPTED_QV_RESULTS='OK,SW_HARDENING_NEEDED' → mask=0x3
Using listening port: 127.0.0.1:9088s
[New Thread 0x7ffff7e97640 (LWP 524482)]
[New Thread 0x7ffff7e7d640 (LWP 524483)]
[New Thread 0x7ffff7e63640 (LWP 524484)]
[New Thread 0x7ffff755b640 (LWP 524485)]
[New Thread 0x7ffff7541640 (LWP 524486)]
Server started on port 9088 (HTTPS)
API available at: https://localhost:9088/v1
OpenAPI documentation: https://localhost:9088/docs/openapi.yaml
Version info: https://localhost:9088/version

Thread 2 "civetweb-worker" received signal SIGSEGV, Segmentation fault.
[Switching to Thread 0x7ffff7e97640 (LWP 524482)]
0x0000000000000000 in ?? ()
=== bt ===#0  0x0000000000000000 in ?? ()
#1  0x000055555560ee77 in ssl_get_client_cert_info ()
#2  0x00005555556134ce in worker_thread_run ()
#3  0x00005555556137a0 in worker_thread ()
#4  0x00007ffff6894ac3 in start_thread (arg=<optimized out>) at ./nptl/pthread_create.c:442
#5  0x00007ffff69268d0 in clone3 () at ../sysdeps/unix/sysv/linux/x86_64/clone3.S:81
=== info threads ===  Id   Target Id                                            Frame 
  1    Thread 0x7ffff7e98780 (LWP 524479) "perp-dex-server" 0x00007ffff68e57f8 in __GI___clock_nanosleep (clock_id=clock_id@entry=0, flags=flags@entry=0, req=0x7fffffffe690, rem=0x7fffffffe690) at ../sysdeps/unix/sysv/linux/clock_nanosleep.c:78
* 2    Thread 0x7ffff7e97640 (LWP 524482) "civetweb-worker" 0x0000000000000000 in ?? ()
  3    Thread 0x7ffff7e7d640 (LWP 524483) "civetweb-worker" __futex_abstimed_wait_common64 (private=0, cancel=true, abstime=0x0, op=393, expected=0, futex_word=0x5555556cfb70) at ./nptl/futex-internal.c:57
  4    Thread 0x7ffff7e63640 (LWP 524484) "civetweb-worker" __futex_abstimed_wait_common64 (private=0, cancel=true, abstime=0x0, op=393, expected=0, futex_word=0x5555556cfb70) at ./nptl/futex-internal.c:57
  5    Thread 0x7ffff755b640 (LWP 524485) "civetweb-worker" __futex_abstimed_wait_common64 (private=0, cancel=true, abstime=0x0, op=393, expected=0, futex_word=0x5555556cfb70) at ./nptl/futex-internal.c:57
  6    Thread 0x7ffff7541640 (LWP 524486) "civetweb-master" 0x00007ffff6918c4f in __GI___poll (fds=0x55555579a340, nfds=1, timeout=2000) at ../sysdeps/unix/sysv/linux/poll.c:29
=== current frame ===Stack level 0, frame at 0x7ffff7e96760:
 rip = 0x0; saved rip = 0x55555560ee77
 called by frame at 0x7ffff7e96ce0
 Arglist at 0x7ffff7e96750, args: 
 Locals at 0x7ffff7e96750, Previous frame's sp is 0x7ffff7e96760
 Saved registers:
  rip at 0x7ffff7e96758
=== regs ===rip            0x0                 0x0
rdi            0x7fffec004b80      140737152830336
rsi            0x7ffff7e96d20      140737352658208
rax            0x7fffec004b80      140737152830336
rbp            0x7ffff7e96cd0      0x7ffff7e96cd0
rsp            0x7ffff7e96758      0x7ffff7e96758
```

### Rollback A.6 node-1 → OLD binary — 2026-05-11T09:43:00Z

```
-rw-r--r-- 1 root      root       3213152 Apr 28 15:36 enclave.signed.so
-rwxrwxr-x 1 azureuser azureuser 22435632 Apr 28 20:57 perp-dex-orchestrator
-rwxr-xr-x 1 root      root       6055016 Apr 28 15:36 perp-dex-server

accounts:
=== sha ===
cebf16057ef11223d5cfbc46b6635ef54aaa4553d6cf04530bfd508fbd52ad5d  enclave.signed.so
7d55e6c7f2aba8056481998739cb309b8651aac35c00a5b25f605e5c4e99a8cb  perp-dex-server
```

### Rollback A.7 start + verify — 2026-05-11T09:43:39Z

```
active
inactive
---
mre=4dfe899771bdb3f309771401... ver=0.1.0
=== also orchestrator ===
active
```

## Outcome — Phase A aborted, node-1 rolled back

**Root cause:** civetweb 1.15 (vendored) + libssl3 3.0.2-0ubuntu1.23 ABI mismatch. `ssl_get_client_cert_info` calls NULL function pointer on first TLS connection. Reproduces on both systemd and foreground runs. Not a Path A bug — build infrastructure issue surfaced by libssl3 patch update between Apr 26 (last successful build 2c3d31f) and May 8 (build 58deb57).

**Decision (Andrey 2026-05-11):** A (rollback) → C (upgrade civetweb 1.15 → 1.16+, vendored). Option B (pin libssl3) rejected per feedback_workarounds_are_arch_bugs.md.

**CI gap finding (architectural follow-up):** existing build-gate verifies the binary COMPILES; does not verify it SERVES a TLS connection. Need a deploy-time runtime smoke test that hits /version (with TLS handshake) before declaring build green. Required for BOTH environments — Hetzner (standalone, dev) AND Azure (cluster, testnet). Without this we re-discover deploy-time crashes at PRG-3 time when we should catch them at PR time.

**State after rollback:** all 3 nodes back to MRENCLAVE 4dfe8997... v0.1.0, both services active. Cluster healthy. No data lost (sealed state was wiped on node-1 during failed attempt — restored from backup).

**Next:** civetweb upgrade work in private repo; PRG-3 retry after that. Separate CI-smoke-test design also queued.
