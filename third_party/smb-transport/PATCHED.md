Vendored smb-transport 0.12.1 (MIT, see LICENSE; upstream https://github.com/afiffon/smb-rs) with one change for Just Video:
`TcpTransport::do_connect` sets `TCP_NODELAY`. Without it, multi-part SMB
writes combined with Nagle's algorithm and delayed ACKs cost ~40 ms per request,
capping sequential 1 MiB reads at ~180 Mb/s even on localhost. Upstream candidate.

A second change: `TransportUtils::parse_socket_address` retries a name lookup
three times (mDNS names like `nas.local` sometimes fail to resolve for a moment
on a busy Wi-Fi link) and, if it still fails, uses the address that last
worked for that name in this process.
