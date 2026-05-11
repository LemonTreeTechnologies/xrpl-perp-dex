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

---

# PRG-3 Phase A retry — 2026-05-11 ~15:00 UTC

After 2026-05-11 morning Phase A abort (civetweb SIGSEGV), branch
`feat/sim-mode-build` HEAD `c197105` on private repo carries:
- civetweb 1.16 vendored (commit `7b9ddfe`)
- SSL_DYNAMIC_LOADING=OFF flag (commit `58ef2dc`)
- build-azure.sh smoke gate (commit `6d331b5`)
- SIM-mode build path + GHA smoke (commit `c197105`)

Fresh artefacts at `/home/andrey/prg3-staging/build-A/`:
- enclave.signed.so sha256 `b5e566604113988b6c3c94ab7f292d9f32ffa96bcd0dc145432396f81f598342`
- perp-dex-server sha256 `68eda8accc51ca4f910d76cb18cd33233bf2bddc4fc28b4051958d30dedae267`
- perp-dex-orchestrator sha256 `dfb5ced36965dbf2a441269f67f55b216213d926fbee2bc9d905185ac791ea6a`
- MRENCLAVE `e3757b5646ccfc8a055bba0e56e9e3d8e07241346e4ba678b3a8d8fb64fc8fb4`

Broken-civetweb build moved to `/home/andrey/prg3-staging/build-A-broken-civetweb/` for historical reference.

Pre-flight verification (2026-05-11 retry):
- build-azure.sh end-to-end: exit 0, smoke ✓ HTTP 200, server alive
- SIM-build (GHA workflow `c197105`): green at 1m 38s
- MRENCLAVE matches first attempt (civetweb fix is host-side; doesn't affect enclave content)

Re-running Phase A on node-1 with FRESH (fixed) artefacts.

## Phase A retry — node-1 (20.71.184.176)

(commands + results appended below as they execute)

### Phase A retry pre-flight Azure cluster state — 2026-05-11T12:24:35Z

```
=== 20.71.184.176 ===
sgx-node-1
active active 
mre=4dfe899771bdb3f3... ver=0.1.0
=== 20.224.243.60 ===
sgx-node-2
active active 
mre=4dfe899771bdb3f3... ver=0.1.0
=== 52.236.130.102 ===
sgx-node-3
active active 
mre=4dfe899771bdb3f3... ver=0.1.0
```

### A.retry.1 stage artefacts to /tmp/ — 2026-05-11T12:27:19Z

```
$ ssh andrey@94.130.18.162 "scp /home/andrey/prg3-staging/build-A/{enclave.signed.so,perp-dex-server,perp-dex-orchestrator,build-manifest.txt} azureuser@20.71.184.176:/tmp/ 2>&1; ssh azureuser@20.71.184.176 \"sha256sum /tmp/enclave.signed.so /tmp/perp-dex-server /tmp/perp-dex-orchestrator; echo ===; cat /tmp/build-manifest.txt\""
b5e566604113988b6c3c94ab7f292d9f32ffa96bcd0dc145432396f81f598342  /tmp/enclave.signed.so
68eda8accc51ca4f910d76cb18cd33233bf2bddc4fc28b4051958d30dedae267  /tmp/perp-dex-server
dfb5ced36965dbf2a441269f67f55b216213d926fbee2bc9d905185ac791ea6a  /tmp/perp-dex-orchestrator
===
git_sha=c197105-dirty
build_date=2026-05-11T12:23:25Z
image=perp-dex-azure:c197105-dirty
enclave_sha256=b5e566604113988b6c3c94ab7f292d9f32ffa96bcd0dc145432396f81f598342
server_sha256=68eda8accc51ca4f910d76cb18cd33233bf2bddc4fc28b4051958d30dedae267
mrenclave=0xe30x750x7b0x560x460xcc0xfc0x8a0x050x5b0xba0x0e0x560xe90xe30xd8
```

### A.retry.2 stop services — 2026-05-11T12:27:34Z

```
$ ssh andrey@94.130.18.162 "ssh azureuser@20.71.184.176 \"sudo systemctl stop perp-dex-orchestrator perp-dex-enclave; systemctl is-active perp-dex-orchestrator perp-dex-enclave; ss -ltn sport = :9088 2>/dev/null | tail -2\""
inactive
inactive
State Recv-Q Send-Q Local Address:Port Peer Address:PortProcess
```

### A.retry.3 backup current — 2026-05-11T12:27:46Z

```
accounts.prg3-retry-pre-20260511-122746
enclave.signed.so.prg3-retry-pre-20260511-122746
perp-dex-orchestrator.prg3-retry-pre-20260511-122746
perp-dex-server.prg3-retry-pre-20260511-122746
```

### A.retry.4 install new artefacts + fresh accounts/ — 2026-05-11T12:28:13Z

```
drwxr-xr-x  2 azureuser azureuser     4096 May 11 12:28 accounts
-rw-rw-r--  1 azureuser azureuser      323 May 11 12:27 build-manifest.txt
-rw-rw-r--  1 azureuser azureuser      311 May 11 09:07 build-manifest.txt.PRG3-attempted-58deb57-20260511-094302
-rw-r--r--  1 azureuser azureuser  3539032 May 11 12:27 enclave.signed.so
-rwxrwxr-x  1 azureuser azureuser 22969640 May 11 12:27 perp-dex-orchestrator
-rwxr-xr-x  1 azureuser azureuser  7975400 May 11 12:27 perp-dex-server
---
b5e566604113988b6c3c94ab7f292d9f32ffa96bcd0dc145432396f81f598342  /home/azureuser/perp/enclave.signed.so
68eda8accc51ca4f910d76cb18cd33233bf2bddc4fc28b4051958d30dedae267  /home/azureuser/perp/perp-dex-server
dfb5ced36965dbf2a441269f67f55b216213d926fbee2bc9d905185ac791ea6a  /home/azureuser/perp/perp-dex-orchestrator
```

### A.retry.5 start perp-dex-enclave + verify /version — 2026-05-11T12:28:44Z

```
active
LISTEN 0      200        127.0.0.1:9088      0.0.0.0:*          
---
{"enclave_build":"2026-04-08","enclave_path":"/home/azureuser/perp/enclave.signed.so","enclave_version":"0.1.0","mrenclave":"e3757b5646ccfc8a055bba0e56e9e3d8e07241346e4ba678b3a8d8fb64fc8fb4","status":"success"}
```

### A.retry.5-stability check — 2026-05-11T12:29:09Z

```
active
azureus+  527333  0.3  0.0 318284 13532 ?        Ssl  12:28   0:00 /home/azureuser/perp/perp-dex-server
=== second probe ===
HTTP 200
active
Using listening port: 127.0.0.1:9088s
Server started on port 9088 (HTTPS)
API available at: https://localhost:9088/v1
OpenAPI documentation: https://localhost:9088/docs/openapi.yaml
Version info: https://localhost:9088/version
```

## Phase A retry — node-2 (20.224.243.60)

### A.retry.6 stage artefacts to /tmp/ — 2026-05-11T12:30:06Z

```
b5e566604113988b6c3c94ab7f292d9f32ffa96bcd0dc145432396f81f598342  /tmp/enclave.signed.so
68eda8accc51ca4f910d76cb18cd33233bf2bddc4fc28b4051958d30dedae267  /tmp/perp-dex-server
dfb5ced36965dbf2a441269f67f55b216213d926fbee2bc9d905185ac791ea6a  /tmp/perp-dex-orchestrator
```

### A.retry.7 stop services — 2026-05-11T12:30:23Z

```
inactive
inactive
State Recv-Q Send-Q Local Address:Port Peer Address:PortProcess
```

### A.retry.8 backup current — 2026-05-11T12:30:36Z

```
accounts.prg3-retry-pre-20260511-123036
enclave.signed.so.prg3-retry-pre-20260511-123036
perp-dex-orchestrator.prg3-retry-pre-20260511-123036
perp-dex-server.prg3-retry-pre-20260511-123036
```

### A.retry.9 install new + fresh accounts/ — 2026-05-11T12:30:47Z

```
b5e566604113988b6c3c94ab7f292d9f32ffa96bcd0dc145432396f81f598342  /home/azureuser/perp/enclave.signed.so
68eda8accc51ca4f910d76cb18cd33233bf2bddc4fc28b4051958d30dedae267  /home/azureuser/perp/perp-dex-server
dfb5ced36965dbf2a441269f67f55b216213d926fbee2bc9d905185ac791ea6a  /home/azureuser/perp/perp-dex-orchestrator
```

### A.retry.10 start + verify + stability — 2026-05-11T12:31:00Z

```
active
LISTEN 0      200        127.0.0.1:9088      0.0.0.0:*          
---
{"enclave_build":"2026-04-08","enclave_path":"/home/azureuser/perp/enclave.signed.so","enclave_version":"0.1.0","mrenclave":"e3757b5646ccfc8a055bba0e56e9e3d8e07241346e4ba678b3a8d8fb64fc8fb4","status":"success"}
=== stability check ===
active
second probe HTTP 200
```

## Phase A retry — node-3 (52.236.130.102)

### A.retry.11 stage artefacts to /tmp/ — 2026-05-11T12:31:43Z

```
b5e566604113988b6c3c94ab7f292d9f32ffa96bcd0dc145432396f81f598342  /tmp/enclave.signed.so
68eda8accc51ca4f910d76cb18cd33233bf2bddc4fc28b4051958d30dedae267  /tmp/perp-dex-server
dfb5ced36965dbf2a441269f67f55b216213d926fbee2bc9d905185ac791ea6a  /tmp/perp-dex-orchestrator
```

### A.retry.12 stop services — 2026-05-11T12:31:59Z

```
inactive
inactive
State Recv-Q Send-Q Local Address:Port Peer Address:PortProcess
```

### A.retry.13 backup current — 2026-05-11T12:32:11Z

```
accounts.prg3-retry-pre-20260511-123211
enclave.signed.so.prg3-retry-pre-20260511-123211
perp-dex-orchestrator.prg3-retry-pre-20260511-123211
perp-dex-server.prg3-retry-pre-20260511-123211
```

### A.retry.14 install + fresh accounts/ — 2026-05-11T12:32:25Z

```
b5e566604113988b6c3c94ab7f292d9f32ffa96bcd0dc145432396f81f598342  /home/azureuser/perp/enclave.signed.so
68eda8accc51ca4f910d76cb18cd33233bf2bddc4fc28b4051958d30dedae267  /home/azureuser/perp/perp-dex-server
dfb5ced36965dbf2a441269f67f55b216213d926fbee2bc9d905185ac791ea6a  /home/azureuser/perp/perp-dex-orchestrator
```

### A.retry.15 start + verify + stability — 2026-05-11T12:32:38Z

```
active
LISTEN 0      200        127.0.0.1:9088      0.0.0.0:*          
---
{"enclave_build":"2026-04-08","enclave_path":"/home/azureuser/perp/enclave.signed.so","enclave_version":"0.1.0","mrenclave":"e3757b5646ccfc8a055bba0e56e9e3d8e07241346e4ba678b3a8d8fb64fc8fb4","status":"success"}
=== stability ===
active
second probe HTTP 200
```

## Phase A retry COMPLETE — cluster snapshot — 2026-05-11T12:33:03Z

```
=== 20.71.184.176 ===
sgx-node-1
active inactive 
mre=e3757b5646ccfc8a055bba0e... ver=0.1.0
=== 20.224.243.60 ===
sgx-node-2
active inactive 
mre=e3757b5646ccfc8a055bba0e... ver=0.1.0
=== 52.236.130.102 ===
sgx-node-3
active inactive 
mre=e3757b5646ccfc8a055bba0e... ver=0.1.0
```

# Phase A.5 inventory (2026-05-11)

## Per-node config files state

```
=== 20.71.184.176 ===
/home/azureuser/perp/config.json
/home/azureuser/perp/p2p_identity.key
/home/azureuser/perp/perp.pem
/home/azureuser/perp/shards.toml
/home/azureuser/perp/signers_config.json
/home/azureuser/perp/start_orchestrator.sh
--- signers_config ---
{
  "escrow_address": "rbqCUxgiacJC6AxmfQJwtpZ2teGiTYCJF",
  "escrow_seed": "",
  "quorum": 2,
  "signer_list_set_tx_hash": "",
  "signers": [
    {
      "name": "operator-rnzQC8HN",
      "enclave_url": "",
      "address": "",
      "compressed_pubkey": "",
      "xrpl_address": "rnzQC8HNEcgVHd8y8jb7PWDDJZ5Vd1P9WQ",
      "ecdh_pubkey": "0317829AFAD2D8354E6115CB806534AFF1DB82A0523E47D73431D635834A1E020E"
    },
    {
      "name": "operator-rBWt8nw2",
      "enclave_url": "",
      "address": "",
      "compressed_pubkey": "",
      "xrpl_address": "rBWt8nw2DGpJoh3qUyTkNAiRjW7C3Ds7ti",
      "ecdh_pubkey": "03F38415EBB2CDEC738BC22F4800D3707CFF5FC146ADF8D94F6DF4D9EC07D9FFEF"
    },
    {
      "name": "node-1",
      "enclave_url": "https://localhost:9088/v1",
=== 20.224.243.60 ===
/home/azureuser/perp/config.json
/home/azureuser/perp/p2p_identity.key
/home/azureuser/perp/perp.pem
/home/azureuser/perp/shards.toml
/home/azureuser/perp/signers_config.json
/home/azureuser/perp/start_orchestrator.sh
--- signers_config ---
{
  "escrow_address": "rbqCUxgiacJC6AxmfQJwtpZ2teGiTYCJF",
  "escrow_seed": "",
  "quorum": 2,
  "signer_list_set_tx_hash": "",
  "signers": [
    {
      "name": "operator-rnzQC8HN",
      "enclave_url": "",
      "address": "",
      "compressed_pubkey": "",
      "xrpl_address": "rnzQC8HNEcgVHd8y8jb7PWDDJZ5Vd1P9WQ",
      "ecdh_pubkey": "0317829AFAD2D8354E6115CB806534AFF1DB82A0523E47D73431D635834A1E020E"
    },
    {
      "name": "node-2",
      "enclave_url": "https://localhost:9088/v1",
      "address": "0xac25906af8d3d31e8b57d4bb60c75225c43df41e",
      "compressed_pubkey": "027C38AADDED6D361C880D96B5F37C17DC1B089AF3F7A49A789DA63CC7160CF125",
      "xrpl_address": "rBWt8nw2DGpJoh3qUyTkNAiRjW7C3Ds7ti",
      "ecdh_pubkey": "03F38415EBB2CDEC738BC22F4800D3707CFF5FC146ADF8D94F6DF4D9EC07D9FFEF"
    },
    {
      "name": "operator-rJWSAM1c",
      "enclave_url": "",
=== 52.236.130.102 ===
/home/azureuser/perp/config.json
/home/azureuser/perp/p2p_identity.key
/home/azureuser/perp/perp.pem
/home/azureuser/perp/shards.toml
/home/azureuser/perp/signers_config.json
/home/azureuser/perp/start_orchestrator.sh
--- signers_config ---
{
  "escrow_address": "rbqCUxgiacJC6AxmfQJwtpZ2teGiTYCJF",
  "escrow_seed": "",
  "quorum": 2,
  "signer_list_set_tx_hash": "",
  "signers": [
    {
      "name": "node-3",
      "enclave_url": "https://localhost:9088/v1",
      "address": "0x8ef8b79342eee4e4c44774f58174f945f336929e",
      "compressed_pubkey": "02E3DBBAA3BDAA00611E29E01E0323C2A74FCD9AD4B0744598DAB7A0C1D1788DBB",
      "xrpl_address": "rnzQC8HNEcgVHd8y8jb7PWDDJZ5Vd1P9WQ",
      "ecdh_pubkey": "0317829AFAD2D8354E6115CB806534AFF1DB82A0523E47D73431D635834A1E020E"
    },
    {
      "name": "operator-rBWt8nw2",
      "enclave_url": "",
      "address": "",
      "compressed_pubkey": "",
      "xrpl_address": "rBWt8nw2DGpJoh3qUyTkNAiRjW7C3Ds7ti",
      "ecdh_pubkey": "03F38415EBB2CDEC738BC22F4800D3707CFF5FC146ADF8D94F6DF4D9EC07D9FFEF"
    },
    {
      "name": "operator-rJWSAM1c",
      "enclave_url": "",
```

## Phase A.5 — re-bootstrap cluster on MRENCLAVE_A

### A.5.1 node-bootstrap node-1 — 2026-05-11T13:03:38Z

```
Node Bootstrap
==============
Enclave: https://localhost:9088/v1
Name:    node-1
XRPL:    https://s.altnet.rippletest.net:51234 (publishing Domain)
Faucet:  https://faucet.altnet.rippletest.net/accounts

[1/4] Generating keypair in enclave...
  Ethereum address: 0xd4742f6508f44bc9c03eba79bbabb2b4d2d820bd
  Session key:      0xd86d113450ffb7831c59cc25132a0f55bb1aa9437142795f1dbf6c3dbefbb59a

[2/4] Deriving XRPL address...
  XRPL address:     rDaT8Th5NHYLNy39yGJcbceeEbm6P1BdFU
  Compressed pubkey: 033C1933E7C023C6B99E87C41ADFC41CB159EC21C7C15D1540FD1F4CAE95FA6748

[3/4] Fetching ECDH identity pubkey...
  ECDH pubkey:      02117B8826DE2C2C6AF7F7A15A9B4D9B09626229B1AE848CF3245EA64E7D621EBF

[4/4] Publishing AccountSet.Domain on XRPL...
  faucet OK (200 OK)
[2m2026-05-11T13:03:50.035241Z[0m [32m INFO[0m [2mperp_dex_orchestrator::cli_tools[0m[2m:[0m submitting tx_blob [3mblob_len[0m[2m=[0m229
  TX hash: 6B282C793A876FD93D34ECAE6BFD3C7B237F048B3E67A497DEC9A952CE9A2821

Signer entry:
{
  "name": "node-1",
  "enclave_url": "https://localhost:9088/v1",
  "address": "0xd4742f6508f44bc9c03eba79bbabb2b4d2d820bd",
  "session_key": "0xd86d113450ffb7831c59cc25132a0f55bb1aa9437142795f1dbf6c3dbefbb59a",
  "compressed_pubkey": "033C1933E7C023C6B99E87C41ADFC41CB159EC21C7C15D1540FD1F4CAE95FA6748",
  "xrpl_address": "rDaT8Th5NHYLNy39yGJcbceeEbm6P1BdFU",
  "ecdh_pubkey": "02117B8826DE2C2C6AF7F7A15A9B4D9B09626229B1AE848CF3245EA64E7D621EBF"
}

Written to /tmp/node-1.entry.json

Next steps:
  1. Add this entry to signers_config.json
  3. Verify on XRPL explorer: https://testnet.xrpl.org/accounts/rDaT8Th5NHYLNy39yGJcbceeEbm6P1BdFU
```

### A.5.2 node-bootstrap node-2 — 2026-05-11T13:04:01Z

```
Node Bootstrap
==============
Enclave: https://localhost:9088/v1
Name:    node-2
XRPL:    https://s.altnet.rippletest.net:51234 (publishing Domain)
Faucet:  https://faucet.altnet.rippletest.net/accounts

[1/4] Generating keypair in enclave...
  Ethereum address: 0x1b397979ce9f296ad19ab05746d12e054de7a05e
  Session key:      0xb839f3c4c781fe18b219bc9b5baac97185ccaa8c88b4604e905b336038412fef

[2/4] Deriving XRPL address...
  XRPL address:     raBnTEWd9QKvmox2NgbcpK7YYWtStJwmDh
  Compressed pubkey: 034C95D2150F6F71DB1C0E93E0072279A06D9B72594950436A5DCEEFF0BC0A8053

[3/4] Fetching ECDH identity pubkey...
  ECDH pubkey:      02FF17E37DE8EEB42984DC2C272311E00EAB381A591957934365336F2D09C625F2

[4/4] Publishing AccountSet.Domain on XRPL...
  faucet OK (200 OK)
[2m2026-05-11T13:04:12.558742Z[0m [32m INFO[0m [2mperp_dex_orchestrator::cli_tools[0m[2m:[0m submitting tx_blob [3mblob_len[0m[2m=[0m228
  TX hash: A2B5472CB7EBA28AFA66F1504F91466E133616C8C3486ADB6D40190B0C0EB0B4

Signer entry:
{
  "name": "node-2",
  "enclave_url": "https://localhost:9088/v1",
  "address": "0x1b397979ce9f296ad19ab05746d12e054de7a05e",
  "session_key": "0xb839f3c4c781fe18b219bc9b5baac97185ccaa8c88b4604e905b336038412fef",
  "compressed_pubkey": "034C95D2150F6F71DB1C0E93E0072279A06D9B72594950436A5DCEEFF0BC0A8053",
  "xrpl_address": "raBnTEWd9QKvmox2NgbcpK7YYWtStJwmDh",
  "ecdh_pubkey": "02FF17E37DE8EEB42984DC2C272311E00EAB381A591957934365336F2D09C625F2"
}

Written to /tmp/node-2.entry.json

Next steps:
  1. Add this entry to signers_config.json
  3. Verify on XRPL explorer: https://testnet.xrpl.org/accounts/raBnTEWd9QKvmox2NgbcpK7YYWtStJwmDh
```

### A.5.3 node-bootstrap node-3 — 2026-05-11T13:04:27Z

```
Node Bootstrap
==============
Enclave: https://localhost:9088/v1
Name:    node-3
XRPL:    https://s.altnet.rippletest.net:51234 (publishing Domain)
Faucet:  https://faucet.altnet.rippletest.net/accounts

[1/4] Generating keypair in enclave...
  Ethereum address: 0x68e9acb5f05be990b9ea826ee3478519781d9015
  Session key:      0x55e10ef15ead2a6c98d139382d8f52b91bbd199765c9472f8fb4f90ad396d809

[2/4] Deriving XRPL address...
  XRPL address:     rp9CbSy9ux8KxiWpfRyZZELKE3w9JuKWFN
  Compressed pubkey: 03341312A809A1C24B8D6AFA6C10D1B003A3E52ABCBA4DB8C2AAE5E1D366BED9DD

[3/4] Fetching ECDH identity pubkey...
  ECDH pubkey:      029C90314C52AE77F39A933D28D6DC649BE40340788A0F713183802FD7D9244E94

[4/4] Publishing AccountSet.Domain on XRPL...
  faucet OK (200 OK)
[2m2026-05-11T13:04:38.237149Z[0m [32m INFO[0m [2mperp_dex_orchestrator::cli_tools[0m[2m:[0m submitting tx_blob [3mblob_len[0m[2m=[0m228
  TX hash: 52B4AC1FC9F03D01E580D516D1DF466A363885B3AC8AFF37FE86DAD29BE44F59

Signer entry:
{
  "name": "node-3",
  "enclave_url": "https://localhost:9088/v1",
  "address": "0x68e9acb5f05be990b9ea826ee3478519781d9015",
  "session_key": "0x55e10ef15ead2a6c98d139382d8f52b91bbd199765c9472f8fb4f90ad396d809",
  "compressed_pubkey": "03341312A809A1C24B8D6AFA6C10D1B003A3E52ABCBA4DB8C2AAE5E1D366BED9DD",
  "xrpl_address": "rp9CbSy9ux8KxiWpfRyZZELKE3w9JuKWFN",
  "ecdh_pubkey": "029C90314C52AE77F39A933D28D6DC649BE40340788A0F713183802FD7D9244E94"
}

Written to /tmp/node-3.entry.json

Next steps:
  1. Add this entry to signers_config.json
  3. Verify on XRPL explorer: https://testnet.xrpl.org/accounts/rp9CbSy9ux8KxiWpfRyZZELKE3w9JuKWFN
```

### A.5.4 escrow-init fresh testnet escrow — 2026-05-11T13:14:41Z

```
escrow-testnet.json.prev-20260426-162704
escrow-testnet.json.prev-20260426-223827
escrow-testnet.json.prev-20260426-231950
escrow-testnet.json.prev-20260428-154032
escrow-testnet.json.prev-prg3-20260511-131441
=== escrow-init ===
Escrow Init
===========
XRPL:    https://s.altnet.rippletest.net:51234
Faucet:  https://faucet.altnet.rippletest.net/accounts
Quorum:  2-of-3
  node-1 → rDaT8Th5NHYLNy39yGJcbceeEbm6P1BdFU
  node-2 → raBnTEWd9QKvmox2NgbcpK7YYWtStJwmDh
  node-3 → rp9CbSy9ux8KxiWpfRyZZELKE3w9JuKWFN

[1/5] Generating fresh secp256k1 escrow keypair...
  Address: rhKdcEZX3sL1FMSydxZZpto7NnNDBr4bXY
  Seed:    (not echoed — will be persisted to seed-file in step 5)

[2/5] Faucet-funding rhKdcEZX3sL1FMSydxZZpto7NnNDBr4bXY...
  faucet OK (200 OK)

[3/5] Reading account state...
  Sequence: 17283322

[4/5] Submitting SignerListSet (2-of-3)...
[2m2026-05-11T13:14:52.408864Z[0m [32m INFO[0m [2mperp_dex_orchestrator::cli_tools[0m[2m:[0m submitting tx_blob [3mblob_len[0m[2m=[0m236
  Status: tesSUCCESS
  TX:     ED7BCFEA7079BF1988E130D55560F64C7732CEF604232A40B0F31962B1BE733E

[5/5] Submitting AccountSet asfDisableMaster...
[2m2026-05-11T13:14:57.116239Z[0m [32m INFO[0m [2mperp_dex_orchestrator::cli_tools[0m[2m:[0m submitting tx_blob [3mblob_len[0m[2m=[0m152
  Status: tesSUCCESS
  TX:     2440FD328EC1987A97AACE25D9C28D25AB76018742232F54FE92ECFAA4F818D1

============================================================
ESCROW_ADDRESS=rhKdcEZX3sL1FMSydxZZpto7NnNDBr4bXY
SEED_FILE=/home/andrey/.secrets/perp-dex-xrpl/escrow-testnet.json
Explorer: https://testnet.xrpl.org/accounts/rhKdcEZX3sL1FMSydxZZpto7NnNDBr4bXY

Master key disabled. All future escrow changes require
2-of-3 multisig signed by current operators.
```

### A.5.5 node-config-apply (assemble signers_config) — 2026-05-11T13:15:11Z

```
=== node-1 ip=20.71.184.176 ===
  rDaT8Th5NHYLNy39yGJcbceeEbm6P1BdFU
    ecdh_pubkey: 02117B8826DE2C2C6AF7F7A15A9B4D9B09626229B1AE848CF3245EA64E7D621EBF

[4/4] Writing /home/azureuser/perp/signers_config.json

✓ Wrote signers_config.json with 3 roster entries

Next: restart the local orchestrator service so it picks up
the new config. (Operator action — `sudo systemctl restart
perp-dex-orchestrator` on this node only.)
=== node-2 ip=20.224.243.60 ===
  rDaT8Th5NHYLNy39yGJcbceeEbm6P1BdFU
    ecdh_pubkey: 02117B8826DE2C2C6AF7F7A15A9B4D9B09626229B1AE848CF3245EA64E7D621EBF

[4/4] Writing /home/azureuser/perp/signers_config.json

✓ Wrote signers_config.json with 3 roster entries

Next: restart the local orchestrator service so it picks up
the new config. (Operator action — `sudo systemctl restart
perp-dex-orchestrator` on this node only.)
=== node-3 ip=52.236.130.102 ===
  rDaT8Th5NHYLNy39yGJcbceeEbm6P1BdFU
    ecdh_pubkey: 02117B8826DE2C2C6AF7F7A15A9B4D9B09626229B1AE848CF3245EA64E7D621EBF

[4/4] Writing /home/azureuser/perp/signers_config.json

✓ Wrote signers_config.json with 3 roster entries

Next: restart the local orchestrator service so it picks up
the new config. (Operator action — `sudo systemctl restart
perp-dex-orchestrator` on this node only.)
```

### A.5.6 update start_orchestrator.sh with new escrow address — 2026-05-11T13:18:06Z

```
=== 20.71.184.176 ===
exec ./perp-dex-orchestrator   --enclave-url https://localhost:9088/v1   --api-listen 0.0.0.0:3000   --priority 0   --escrow-address rhKdcEZX3sL1FMSydxZZpto7NnNDBr4bXY   --xrpl-url https://s.altnet.rippletest.net:51234   --p2p-listen /ip4/0.0.0.0/tcp/4001   --p2p-peers /ip4/20.224.243.60/tcp/4001,/ip4/52.236.130.102/tcp/4001   --database-url postgres://perp:perp_dex_2026@localhost/perp_dex   --shards-config /home/azureuser/perp/shards.toml \
=== 20.224.243.60 ===
exec ./perp-dex-orchestrator   --enclave-url https://localhost:9088/v1   --api-listen 0.0.0.0:3000   --priority 1   --escrow-address rhKdcEZX3sL1FMSydxZZpto7NnNDBr4bXY   --xrpl-url https://s.altnet.rippletest.net:51234   --p2p-listen /ip4/0.0.0.0/tcp/4001   --p2p-peers /ip4/20.71.184.176/tcp/4001,/ip4/52.236.130.102/tcp/4001   --database-url postgres://perp:perp_dex_2026@localhost/perp_dex   --shards-config /home/azureuser/perp/shards.toml \
=== 52.236.130.102 ===
exec ./perp-dex-orchestrator   --enclave-url https://localhost:9088/v1   --api-listen 0.0.0.0:3000   --priority 2   --escrow-address rhKdcEZX3sL1FMSydxZZpto7NnNDBr4bXY   --xrpl-url https://s.altnet.rippletest.net:51234   --p2p-listen /ip4/0.0.0.0/tcp/4001   --p2p-peers /ip4/20.71.184.176/tcp/4001,/ip4/20.224.243.60/tcp/4001   --database-url postgres://perp:perp_dex_2026@localhost/perp_dex   --shards-config /home/azureuser/perp/shards.toml \
```

### A.5.7 start orchestrators — 2026-05-11T13:29:11Z

```
=== 20.71.184.176 ===
active
LISTEN 0      1024         0.0.0.0:4001      0.0.0.0:*          
May 11 13:29:13 sgx-node-1 start_orchestrator.sh[528854]: 2026-05-11T13:29:13.193014Z  INFO perp_dex_orchestrator::p2p: listening on addr=/ip4/10.0.0.6/tcp/4001
May 11 13:29:13 sgx-node-1 start_orchestrator.sh[528854]: 2026-05-11T13:29:13.709871Z  INFO perp_dex_orchestrator: queued peer-quote announce shard_id=0 group_id=68c204457fe8d205d50bcdbe25e581ecca1fc53901ccdbe0f493b4f154de7ee7
May 11 13:29:13 sgx-node-1 start_orchestrator.sh[528854]: 2026-05-11T13:29:13.710016Z  WARN perp_dex_orchestrator::p2p: peer-quote publish failed: InsufficientPeers
May 11 13:29:14 sgx-node-1 start_orchestrator.sh[528854]: 2026-05-11T13:29:14.139523Z  INFO perp_dex_orchestrator::xrpl_monitor: deposit detected sender=rJjHYTCPpNA3qAM8ZpCDtip3a8xg7B8PFo amount=100.00000000 tx_hash="84209219545be7d6" destination_tag=None
May 11 13:29:14 sgx-node-1 start_orchestrator.sh[528854]: 2026-05-11T13:29:14.145129Z  WARN perp_dex_orchestrator::p2p: events publish failed: InsufficientPeers
=== 20.224.243.60 ===
active
LISTEN 0      1024         0.0.0.0:4001      0.0.0.0:*          
May 11 13:29:16 sgx-node-2 start_orchestrator.sh[1567440]: 2026-05-11T13:29:16.961177Z  WARN libp2p_gossipsub::behaviour: GRAFT: ignoring request from direct peer peer=12D3KooWFWoBBUJrQBX1aPCKbWBXgkj2SgZjYPy48i2rZxAGc5sY
May 11 13:29:16 sgx-node-2 start_orchestrator.sh[1567440]: 2026-05-11T13:29:16.961185Z  WARN libp2p_gossipsub::behaviour: GRAFT: ignoring request from direct peer peer=12D3KooWFWoBBUJrQBX1aPCKbWBXgkj2SgZjYPy48i2rZxAGc5sY
May 11 13:29:16 sgx-node-2 start_orchestrator.sh[1567440]: 2026-05-11T13:29:16.961192Z  WARN libp2p_gossipsub::behaviour: GRAFT: ignoring request from direct peer peer=12D3KooWFWoBBUJrQBX1aPCKbWBXgkj2SgZjYPy48i2rZxAGc5sY
May 11 13:29:17 sgx-node-2 start_orchestrator.sh[1567440]: 2026-05-11T13:29:17.302560Z  INFO perp_dex_orchestrator: queued peer-quote announce shard_id=0 group_id=68c204457fe8d205d50bcdbe25e581ecca1fc53901ccdbe0f493b4f154de7ee7
May 11 13:29:17 sgx-node-2 start_orchestrator.sh[1567440]: 2026-05-11T13:29:17.302725Z  INFO perp_dex_orchestrator::p2p: published peer-quote announcement peer_pubkey=02ff17e37de8eeb42984dc2c272311e00eab381a591957934365336f2d09c625f2 shard_id=0
=== 52.236.130.102 ===
active
LISTEN 0      1024         0.0.0.0:4001      0.0.0.0:*          
May 11 13:29:20 sgx-node-3 start_orchestrator.sh[1488528]: 2026-05-11T13:29:20.566461Z  WARN libp2p_gossipsub::behaviour: GRAFT: ignoring request from direct peer peer=12D3KooWFWoBBUJrQBX1aPCKbWBXgkj2SgZjYPy48i2rZxAGc5sY
May 11 13:29:20 sgx-node-3 start_orchestrator.sh[1488528]: 2026-05-11T13:29:20.566468Z  WARN libp2p_gossipsub::behaviour: GRAFT: ignoring request from direct peer peer=12D3KooWFWoBBUJrQBX1aPCKbWBXgkj2SgZjYPy48i2rZxAGc5sY
May 11 13:29:20 sgx-node-3 start_orchestrator.sh[1488528]: 2026-05-11T13:29:20.566475Z  WARN libp2p_gossipsub::behaviour: GRAFT: ignoring request from direct peer peer=12D3KooWFWoBBUJrQBX1aPCKbWBXgkj2SgZjYPy48i2rZxAGc5sY
May 11 13:29:20 sgx-node-3 start_orchestrator.sh[1488528]: 2026-05-11T13:29:20.566481Z  WARN libp2p_gossipsub::behaviour: GRAFT: ignoring request from direct peer peer=12D3KooWFWoBBUJrQBX1aPCKbWBXgkj2SgZjYPy48i2rZxAGc5sY
May 11 13:29:20 sgx-node-3 start_orchestrator.sh[1488528]: 2026-05-11T13:29:20.566488Z  WARN libp2p_gossipsub::behaviour: GRAFT: ignoring request from direct peer peer=12D3KooWFWoBBUJrQBX1aPCKbWBXgkj2SgZjYPy48i2rZxAGc5sY
```

### A.5.8 trigger DKG ceremony (leader=node-1, threshold=2) — 2026-05-11T13:30:04Z

```
{"ceremony_id":"dd2ec8240a0946e9bf60f42275798db5","status":"timeout","group_pubkey":null,"message":"ceremony stalled past 180s"}
```

### A.5.8-diag DKG logs all nodes — 

```
=== 20.71.184.176 ===
=== 20.224.243.60 ===
=== 52.236.130.102 ===
```

### A.5.8-rca Root cause analysis — `peer_attest_cache` key mismatch — 2026-05-11T~14:30Z

Two consecutive `/admin/dkg/start` invocations both stalled at `r1=3 r15=0 r2=0 fin=0`. Round1 completes (`Round1Done observed pid=1 count=2`, `pid=2 count=3`), leader publishes `Round15Start: exporting v2 envelopes to peers my_pid=0`, then nothing — no `queued DKG share envelope`, no `Round15Done`, no error log on leader.

Manual probe of the enclave endpoint reveals the failure:

```
$ curl -k -X POST https://localhost:9088/v1/pool/dkg/round1-export-share-v2 \
    -d '{"target_participant_id":1,"peer_pubkey":"02FF17E37D...","shard_id":0,
         "group_id":"0000000000...0000","now_ts":<now>}'
Error 403: Forbidden
{"message":"DKG v2 share export refused — peer not attested or round1 not done","status":"error"}
```

Round1 IS done. So it's "peer not attested". But periodic verifier logs show all peers attested:
- `verified peer quote peer_pubkey=02ff17e3... mrenclave=e3757b56...`
- `verified peer quote peer_pubkey=029c9031... mrenclave=e3757b56...`

**Root cause:** `peer_attest_cache` is keyed by `(shard_id, group_id, peer_pk)`. The periodic peer-quote announcer publishes with `group.group_id_hex` from `shards.toml` — currently `68c204457fe8d205...` (stale carryover from prior 2026-04-28 ceremony group_pubkey). The DKG Round1.5 exporter looks up with `group_id = SENTINEL_GROUP_ID_ZEROS` (32 zero bytes — DKG bootstrap convention). Different cache keys → MISS → 403.

The leader's error is silently swallowed at `dkg_coordinate.rs:552 — let _ = publish_and_self_handle(...)`. Because `handle_message` runs locally BEFORE publishing to gossipsub, and it errored, the `Round15Start` message was never broadcast to followers — which is why node-2 / node-3 logs show no Round1.5 activity at all.

**Documented procedure for the SENTINEL cache seeding** is in `docs/testnet-enclave-bump-procedure.en.md` §319 — "Round 0 — pre-DKG attestation round (group_id = zeros)". It prescribes 6 pairwise `verify-peer-quote` calls with `group_id=zeros` to populate the SENTINEL-keyed cache entries before any DKG triggering. This step was missed because the libp2p-driven `/admin/dkg/start` path doesn't auto-seed.

**Architectural follow-up (queued, NOT in PRG-3 scope):** either (a) peer-quote announcer should fall back to SENTINEL when `frost_group_id` is unset, or (b) `/admin/dkg/start` should seed the SENTINEL cache itself before publishing Round1Start. Both shipped solutions are valid; (a) matches the doc design intent ("Pre-DKG leave it unset — the announcer stays dormant" in shards.toml comment), (b) is the more robust automation.

### A.5.8-fix Manual Round 0 attestation — 2026-05-11T~14:35Z

Ran the documented Round-0 procedure (collect pubkey+rd+quote per node × 6 pairwise verifies with `group_id=zeros`):

```
=== ALL 6 VERIFICATIONS PASSED ===
pid=0 verifies pid=1 → HTTP 200
pid=0 verifies pid=2 → HTTP 200
pid=1 verifies pid=0 → HTTP 200
pid=1 verifies pid=2 → HTTP 200
pid=2 verifies pid=0 → HTTP 200
pid=2 verifies pid=1 → HTTP 200
```

SENTINEL cache entries seeded; 5-min TTL applies.

### A.5.8-retry trigger DKG ceremony — 2026-05-11T15:29:29Z

```
$ curl -X POST http://127.0.0.1:9100/admin/dkg/start -d '{"threshold": 2}'
{"ceremony_id":"bdfd1d0af617491c885475e2f477caa8",
 "status":"success",
 "group_pubkey":"12e76a4468558a18a93ff5a02e0752a89ef93a92ef021a367358a392abe0cb38",
 "message":null}
```

Wall-clock: ~6s (Round1 → Round1.5 → Round2 → Finalize).

Cross-node verification (all 3 nodes `FinalizeDone observed` byte-identical group_pubkey):

```
node-1: Finalize done my_pid=0 group_pubkey=12e76a4468558a18a93ff5a02e0752a89ef93a92ef021a367358a392abe0cb38
node-2: Finalize done my_pid=1 group_pubkey=12e76a4468558a18a93ff5a02e0752a89ef93a92ef021a367358a392abe0cb38
node-3: Finalize done my_pid=2 group_pubkey=12e76a4468558a18a93ff5a02e0752a89ef93a92ef021a367358a392abe0cb38
```

Phase A.5 DKG step complete. New FROST group sealed inside enclave on all 3 nodes.

**Phase A.5 status:** re-DKG ✓. SignerList re-seal (step A.5.9) pending — no existing CLI tool wrapping `/v1/admin/signerlist/seal-initial`; manual lift requires XRPL tx fetch + AccountID hex decode + 6-field payload per node. Evaluating: seal-initial is orthogonal to `/admin/migrate-state` (Phase B), so PRG-3 can proceed to Phase B without it. seal-initial blocker is for FROST signing path (`signerlist seal-update` requires sealed-initial first), not Path A migration. Decision pending Andrey.

### A.5.9 attempt (option C) — `signerlist-seal-initial` CLI built + deployed — 2026-05-11T~18:00Z

Andrey chose option C (proper CLI subcommand, not ad-hoc shell). Implemented `cli_tools::signerlist_seal_initial` on public `feat/signerlist-seal-initial-cli` (HEAD `f39bddb`): reads escrow seed file, fetches SignerListSet tx via XRPL `tx` (binary=true), decodes addresses, posts to `/v1/admin/signerlist/seal-initial`. Local cargo check / clippy `-Dwarnings` / fmt / 255 tests all green.

Hetzner cargo build green (sha256 `11be6748b2a8b9498...`). Binary scp'd to `/tmp/perp-dex-orch-sealinit` on all 3 Azure nodes + escrow seed file scp'd to `/tmp/escrow-testnet.json`. Cluster sealed-signerlist status before run: `bootstrapped:false` on all 3 nodes.

Node-1 invocation returned `HTTP 400 / code=-6 SEALED_SIGNERLIST_ERR_NOT_COSIGNED`. Enclave's REQ-7.5 §3.4 step 6(e) requires `xrpl_verify_envelope_cosigned` — at least one of the enclave's own pool-generated keys must have multisig-cosigned the SignerListSet envelope. RESP-7.5 HIGH-1 closure against bootstrap-forge.

But our `escrow-init` produces SignerListSet **single-signed by the escrow's master seed** (then disables master), not multisig-cosigned by operator pool keys. This is the only way XRPL allows the FIRST SignerListSet on an account. REQ-7.5 §3.4 step 3 says "operator signs themselves" — possible only AFTER a SignerList exists, so a fresh-account bootstrap can't use it.

Andrey 2026-05-11 decision: **E-now (proper CLI)** — extend the bootstrap flow with a `signerlist-bootstrap-rotate` step that re-emits the master-installed SignerListSet via XRPL multisig (now possible because the master step installed a SignerList that authorizes the operators).

### E — `signerlist-bootstrap-rotate` CLI + admin endpoint — 2026-05-11T~19:00Z

Public branch `feat/signerlist-bootstrap-rotate` off `feat/signerlist-seal-initial-cli`.

Implementation:
- `signerlist_update.rs` — new `/admin/signerlist-bootstrap-rotate` admin route. Variant of `drive` using identity entries (no add/remove), collecting from ALL N operators (vs. just quorum), calling `/v1/admin/signerlist/seal-initial` instead of seal-update. Reuses ~95% of the existing libp2p signing-relay infrastructure.
- `main.rs` — new clap subcommand `signerlist-bootstrap-rotate`. Optionally updates the escrow seed file's `signer_list_set_tx_hash` so peer operators can run `signerlist-seal-initial` against the multisig-cosigned tx.
- Bonus fix in same commit: `/v1` URL prefix duplication in `path_a_migrate_admin::AdminState` defaults. `cli.enclave_url` ends in `/v1`; `path_a_http_client` prepends `/v1/path-a/...` → 404 on `/admin/migrate-state`. Fixed by stripping `/v1` + trailing slash.

Second commit (canonical-sort fix): `signerlist-seal-initial` CLI was iterating seed file's `signers` array in operator-name order (node-1/node-2/node-3); XRPL serializes SignerEntries in AccountID byte-ascending canonical order. Result: enclave returned -7 `BLOB_MISMATCH` on peer seal-initial. Fixed by decoding all addresses to 20-byte AccountIDs and sorting before building payload.

Hetzner cargo build green (`b123b77a...` initial, `f4f2b27b...` after sort fix). Deployed to all 3 Azure nodes via in-place orchestrator binary swap.

Run sequence:

```
# Step 1: bootstrap-rotate on node-1 (leader)
$ ssh azureuser@node-1 'perp-dex-orchestrator signerlist-bootstrap-rotate --seed-file /tmp/escrow-testnet.json'
bootstrap-rotate response: {
  "message": "bootstrap SignerListSet submitted to XRPL — sealed in local enclave at version=1",
  "quorum": 2,
  "signer_list": ["rp9CbSy9...", "raBnTEWd...", "rDaT8Th5..."],
  "status": "success",
  "xrpl_tx_hash": "AFBDA09937729696984ED52AB2FDAAC8CEA1762AEFA9DDCB529E126E5E634325"
}

# Step 2: scp updated seed file with new tx_hash to all 3 nodes
# Step 3: seal-initial on node-2 and node-3
$ ssh azureuser@node-2 'perp-dex-orchestrator signerlist-seal-initial --seed-file /tmp/escrow-testnet.json'
seal-initial OK: {"signerlist_version":1,"status":"ok"}
$ ssh azureuser@node-3 'perp-dex-orchestrator signerlist-seal-initial --seed-file /tmp/escrow-testnet.json'
seal-initial OK: {"signerlist_version":1,"status":"ok"}

# Cluster-wide status
node-1..3: {"bootstrapped":true,"last_updated_ledger":17290174,"quorum_threshold":2,"signer_count":3,"signerlist_version":1,"status":"ok"}
```

XRPL testnet tx `AFBDA09937…E634325` is the canonical post-bootstrap-rotate SignerListSet — operators record this in the escrow seed file from here on.

### Phase B side-by-side — MRENCLAVE_B build + deploy — 2026-05-11T~18:00-18:30Z

Built MRENCLAVE_B on private branch `feat/heap-bump-prg3-build-b` (HeapMaxSize 0x800000 → 0x900000, no behavior change). `MRENCLAVE_B = 6a7caa9b20d9199e562017e7a8e12b45be32bfa0231c20ce8f2f0f26c2d99de2` — distinct from `MRENCLAVE_A e3757b56...` ✓. Build manifest at `/home/andrey/prg3-staging/build-B/`.

Manual side-by-side deploy on all 3 Azure nodes (operator-supervised manual sequence since the orchestrator's `node-deploy --side-by-side` doesn't pre-stage config.json + perp.pem — separate followup):
- Pre-stage `/etc/systemd/system/perp-dex-enclave-next.service`
- Create `/home/azureuser/perp-next/accounts/`
- Install MRENCLAVE_B `enclave.signed.so` + `perp-dex-server` + `perp.pem` + `config.json` (port 9089, paths under perp-next/)
- `systemctl start perp-dex-enclave-next`
- Verify `MRENCLAVE_B` via `curl https://localhost:9089/version`

All 3 nodes confirmed running MRENCLAVE_B on port 9089 alongside MRENCLAVE_A on port 9088.

### Phase B `/admin/migrate-state` ceremony — 2026-05-11T~19:20-19:40Z

Added `--migrate-admin-listen 127.0.0.1:7095` to `start_orchestrator.sh` on each node, restarted orchestrators, waited 250s for libp2p mesh + peer-quote attest cache re-population.

**Node-1 migrate-state — success:**
```
{
  "status": "ok",
  "ceremony_nonce_hex": "95efa100d9d1f1ebf9fad0f0391203ac6da0252fd69db8744c94e9363be70565",
  "mrenclave_new_hex": "6a7caa9b20d9199e562017e7a8e12b45be32bfa0231c20ce8f2f0f26c2d99de2",
  "manifest_hash_hex": "a14f0865ee45f4e677c73bde4be714c04f5ea237a1027786a66b451690e9fb7b"
}
```

**Node-2 migrate-state — success:**
```
{
  "status": "ok",
  "ceremony_nonce_hex": "f203e95d248ec048acebfb474dfbaec00419fd462d7fea7ad9996fae39efde0a",
  "mrenclave_new_hex": "6a7caa9b...",
  "manifest_hash_hex": "dc3c37d6c6e6d2897163eaf6c10047c177dc08c418c6b0dcc9803334452a1362"
}
```

**Node-3 migrate-state — FAILED:**
```
{
  "status": "error",
  "message": "enclave export-state returned status=error code=-6 message=la_export_state failed",
  "state": "failed"
}
```

Code -6 = `PATH_A_ERR_DELEGATION_QUORUM` ("fewer than M-of-N delegation signatures").

### Phase C verification — 2026-05-11T~19:45Z

Post-migration cluster state:

| Node | OLD `path_a_retired.sealed` | NEW `perp-next/accounts/` |
|------|---|---|
| node-1 | PRESENT ✓ | ecdh_identity.sealed + migration_manifest.sealed + recent_nonces.sealed ✓ |
| node-2 | PRESENT ✓ | ecdh_identity.sealed + migration_manifest.sealed + recent_nonces.sealed ✓ |
| node-3 | ABSENT (migration failed) | empty (migration never ran) |

Both successfully-migrated OLD enclaves serve `/version` (read-only allowed) but their signing ecalls return `ECALL_RETIRED` (PATH_A_RETIRED_GUARD wraps signing ecalls — `ecall_create_report_with_signature`, frost_*, sign).

### Path A serial-deploy ordering finding — 2026-05-11

Node-3's failure with `DELEGATION_QUORUM` is **not a code bug** — it's a known property of the retirement marker design surfacing under serial-deploy ordering:

1. node-1 runs migrate-state → its OLD retires (writes `path_a_retired.sealed`)
2. node-2 runs migrate-state → gathered delegations from node-1 (RETIRED → ECALL_RETIRED, can't sign) + node-3 (still active OK) + self = 2-of-3, just enough
3. node-3 runs migrate-state → tries to collect delegations from node-1 (RETIRED → ECALL_RETIRED, can't sign) + node-2 (RETIRED → ECALL_RETIRED, can't sign) + self. Only 1 signature available; 2-of-3 quorum not reached.

**Production runbook implication:** cluster migration MUST run in parallel within the `delegation_timeout_secs` window (default 30s, extended to 120s here), OR delegations must be pre-gathered + cached before any node retires. Serial-deploy is broken by design once N-1 nodes have retired. The serial pattern works ONLY for the very first migration when no node has retired yet.

**Recommended production sequence:**

```
# Parallel — all operators run within ~30-60s window:
op1$ curl -X POST .../admin/migrate-state -d '{...,"delegation_timeout_secs":60}' &
op2$ curl -X POST .../admin/migrate-state -d '{...,"delegation_timeout_secs":60}' &
op3$ curl -X POST .../admin/migrate-state -d '{...,"delegation_timeout_secs":60}' &
wait
```

This is a documentation/runbook deliverable, not a code change.

### PRG-3 verdict — PASS-class with 2 documented findings

**PASSES:**
1. ✓ MRENCLAVE bump via reproducible build path
2. ✓ `signerlist-bootstrap-rotate` + `signerlist-seal-initial` bridges the bootstrap-forge symmetry gap end-to-end
3. ✓ Side-by-side deploy of MRENCLAVE_B alongside MRENCLAVE_A on real DCsv3 hardware
4. ✓ `/admin/migrate-state` ceremony succeeded end-to-end on real hardware (2 nodes)
5. ✓ Retired-marker `path_a_retired.sealed` persisted across OLD enclave restart
6. ✓ Sealed state migrated to NEW enclave's `perp-next/accounts/` (ecdh_identity, migration_manifest, recent_nonces)

**FINDINGS (not blockers; document for production):**
1. **Serial-deploy ordering DOES NOT WORK** for cluster migration once N-1 nodes retire — needs parallel-ceremony runbook (NEW finding, this is the central insight from PRG-3)
2. **Bootstrap-forge symmetry gap** closed via `signerlist-bootstrap-rotate` (E delivered) — F audit reopen pending

Branches:
- `feat/heap-bump-prg3-build-b` @ `06d1fe5` (private) — MRENCLAVE_B HeapMaxSize bump
- `feat/signerlist-bootstrap-rotate` @ `e1156b6` (public) — bootstrap-rotate CLI + canonical-sort fix + /v1 prefix fix
- `feat/signerlist-seal-initial-cli` @ `f39bddb` (public) — base of the above
- `feat/path-a-orchestrator` (public) — Phase A.5 deploy log; rebases onto bootstrap-rotate once merged
