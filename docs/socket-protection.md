# Host socket protection

VPN bypass is a socket-creation requirement, not an operation on a socket that
core has already connected. Core and WASI guests never need an OS file descriptor.

## Portable requests

`TcpBindOptions::need_protect` and `UdpBindOptions::need_protect` travel through the
existing connect/bind operations. A host requiring VPN bypass must acknowledge
successful protection before connect, bind/listen, or publishing the socket.
Failure or cancellation fails creation and discards the owned socket; emitting
an event alone is not acknowledgement. Platforms without VPN bypass can treat
the requirement as a no-op.

- TCP connect constructors request protection, including egress for proxies.
- TCP transport/hole-punch listeners request protection. Hosts retain this flag
  on the listener and protect accepted children before handing them to core.
- Local ProxyNat/SOCKS/port-forward/port-lease listeners do not request it.
- UDP transport, candidates, listeners, NAT egress and STUN request protection;
  HolePunchControl, local SOCKS/port-forward and port-lease sockets do not.
- Low-level TCP/UDP bind options default to protection, including deserialization
  of options without `need_protect`. The named local/TUN-facing constructors set
  `false` explicitly. The UDP default keeps its historical purpose label for
  socket setup; only the named `hole_punch_control()` constructor opts out.
- `with_bind()` replaces the **entire** bind object. A local listener's replacement
  must retain its opt-out rather than inherit the protected default. A native
  adapter must honor explicit `false`, not silently change it based on `purpose`.
- DNS and source-route queries are already host-owned operations. A bypass-enabled
  host must protect their underlying sockets before querying/probing, including
  DNS TCP fallback, rather than letting system DNS silently bypass this contract.

## TUN-facing ingress and port forwarding

The extra `force_smoltcp` wildcard port-forward ingress listener has been removed.
Normal TUN-backed ingress is delivered to the existing unprotected native listener;
its accepted sockets remain unprotected so overlay replies can return through TUN.
The physical/underlay egress socket is protected independently. A separate
DataPlane listener must not mask broken host/TUN routing in this path.

This does not remove the existing public DataPlane listener APIs, the generic
no-TUN smoltcp TCP proxy, or `force_smoltcp` itself. Those have other uses. Whether
Android subnet proxy works without forced smoltcp needs actual platform regression
testing; socket-creation unit tests alone do not establish that result.

## Native integration and HarmonyOS

The native `easytier` adapter implements the creation requirement using an async
`NativeSocketProtector` callback. This is a native implementation detail, not a
new portable or WASI ABI. It takes only the native handle; the creation options
select policy, without a second purpose enum. Namespace switching is confined to synchronous socket
creation and never held across the callback's await.

The existing native `bind` builder is async and shared by TCP/UDP creation. Its
legacy direct-call default remains unprotected; portable factories explicitly
pass their core bind options (default protected). Callers must await `.call()`.
TCP listeners reuse TCP socket creation then listen, instead of duplicating the
setup. WebSocket and FakeTCP listeners use the same protected TCP factory as
other transports. FakeTCP source probes and kernel decoy connections wait for
protection; its packet driver sends through the selected physical interface.

## Desktop capture and socket lifetime

Desktop route installation uses the shared effective CIDR plan. Direct regions,
local advertised subnets and ordinary private bypass regions are removed from
VPN capture. Known mesh and remote advertised regions retain ordinary overlay
routes even after a remote peer becomes unreachable. Exit liveness never removes
its capture region: all unavailable exits block that region in the packet plane.

Socket factories retain a protection registration for the socket's lifetime,
including both owned TCP halves. The registry holds weak references to duplicate
handles; it neither retains closed sockets nor later protects a reused raw FD.
Before takeover it protects every existing registered socket. Registration and
activation are serialized, so a new socket observes the active bypass before
its first I/O. Explicit `need_protect=false` remains unprotected.

Linux uses dedicated capture/reject routing tables per NIC and a shared bypass bit per network
namespace. Protected sockets receive `original_mark | bypass_bit`; existing
marks are preserved, including marks applied by an external protection callback.
The bit must be unused by configured/live socket marks and every external
fwmark mask. An exact external mask or exhausted bit space rejects activation.
Each capture rule uses the positive selectors `iif lo fwmark 0/bit` and its
owned table; the following reject rule prevents an unavailable capture route
from falling through to physical routing. Forwarded exit packets arriving from
TUN skip both rules.
Its priority is after local delivery and before external nonlocal routing;
activation fails when no safe priority exists. Protected sockets skip it and
retain their original external routing. Physical defaults and external policy
rules remain intact. Because `SO_BINDTODEVICE` can skip these routing selectors,
each owner also creates an `ETVPN_<table>` filter chain in both `iptables` and
`ip6tables`, with a comment-identified jump at the start of OUTPUT. Unmarked
capture packets leaving any interface other than TUN are dropped at this hook.
Only known overlay regions allow local delivery; compiled Exit and Block regions
retain that drop even for local destinations. Forwarded packets do not traverse
OUTPUT, and protected sockets skip the guard through the reserved mark bit.
Kernel IPv6 neighbor/router discovery does not consistently inherit the marked
socket's bypass bit. The guard therefore returns only ICMPv6 types 133–136 with
code zero and hop limit 255; router discovery is limited to link-local scopes,
while neighbor discovery also permits global unicast neighbor addresses. These
rows do not permit ordinary UDP or ICMP Echo, and RETURN preserves the host's
remaining OUTPUT firewall rules.
The IPv4 and IPv6 OUTPUT tools are required for Linux client activation; setup
failure is reported instead of activating without the guard. Multiple owners
share the marker; the last release removes it after all owned policy rules and
OUTPUT jumps/rows/chains have been removed. Cleanup deletes exact owned rows and
empty chains, preserving external firewall rules rather than flushing them.

Windows and macOS create protected wildcard transport listeners as a collection
of concrete physical interface listeners from startup. Each listener has its
source/interface scope before `listen`, protecting kernel SYNACKs as well as
accepted sockets. The public listener retains its logical wildcard address and
port. Interface refresh adds new listeners before retiring disappeared addresses
and keeps accepted streams alive. Explicit local binds and unprotected local/TUN
listeners retain their original behavior.

Wildcard UDP reception stays wildcard. The native adapter sets and restores
interface scope only around each synchronous datagram operation; explicit source
metadata can select a secondary physical interface. Outgoing TCP is scoped before
connect, and accepted children are acknowledged before exposure. Physical default
and connected interfaces are refreshed as host networking changes. Unbound
sockets select the target's loopback or longest matching connected physical
network before the physical default. IPv4-mapped IPv6 targets use IPv4 interface
scope, while explicit source and existing interface constraints retain priority.
Protection uses socket
scope/marks rather than blanket endpoint-IP routes.

Initial native capture awaits a readiness acknowledgement. Protection or route
failure fails startup; hot failures appear in `activation_error`, and successful
reconciliation clears that field. Explicit `no_tun` or missing virtual addresses
retain their normal interface lifecycle and report why client capture is absent.
NIC shutdown cancels packet I/O, awaits route owners and forwarding leases, then
drops the virtual interface. Namespace-local netlink operations run on dedicated
threads, never with a `setns` guard suspended across Tokio worker threads.

## Bootstrap and user DNS

Bootstrap/TXT/SRV resolution uses protected native TCP and UDP factories. On Linux,
a systemd-resolved stub is replaced with its published uplinks from
`/run/systemd/resolve/resolv.conf`; protecting a query to the loopback stub cannot
protect systemd-resolved's separate upstream socket. Namespace resolver files are
used when available. This keeps own bootstrap queries on the underlay during an
exit outage.

System DNS capture uses the instance's namespace resolver file when present.
If that resolver uses a systemd stub, capture inputs also include the published
upstream addresses, so narrow Exit rules still protect the stub's user queries.
Policy compilation continues to apply the configured private-network exemption.

## HarmonyOS acknowledgements

The HarmonyOS broker wakes the already-pending request consumer with `Notify`
(no polling timer). It keeps a duplicate FD alive until ArkTS completes
`VpnConnection.protect(fd)` and returns its ACK. The waiting creation future is
woken immediately by the oneshot acknowledgement. Failure stays fail-closed;
shutdown retains dispatched FDs for late ACKs to prevent FD reuse races.
The ArkTS request shape is unchanged; its diagnostic `purpose` string is now
the generic `"socket"`. Neither ACK routing nor protection policy uses that label.

This guarantees ordering, not a wall-clock real-time bound: OS/ArkTS scheduling
can still delay protection. Such a delay keeps the socket unconnected; it must
never allow the first SYN/query to race ahead of protection.

## WASI option format

The `easytier_host` import names and function signatures are unchanged. TCP
connect, TCP listen and UDP bind use option document **version 3** (previously 2):
one `u8` boolean `need_protect` is inserted immediately after the existing purpose
byte and before the optional bind-device field. All other field encodings and
purpose values are unchanged. The document version lets older hosts reject
unsupported options instead of silently ignoring protection. DNS, environment,
instance-config and data-plane layouts/versions are unchanged.

An embedding host must update its versioned option decoder and honor the flag
inside its existing creation implementation. The external host implementation
is not in this repository: building the guest proves propagation/compatibility
of imports, not that every external host has implemented platform protection.
