const IPV4_OCTET = /^\d{1,3}$/;
const CIDR_PREFIX = /^\d{1,2}$/;

function parseIpv4(text: string): [number, number, number, number] | null {
    const octets = text.split('.');
    if (octets.length !== 4) return null;
    const values: [number, number, number, number] = [0, 0, 0, 0];
    for (let i = 0; i < 4; i++) {
        // Digits only: an empty field ("1..2.3", "1.2.3.") must not coerce
        // into an octet, because Number('') === 0.
        if (!IPV4_OCTET.test(octets[i])) return null;
        const value = Number(octets[i]);
        if (value > 255) return null;
        values[i] = value;
    }
    return values;
}

// Normalize user input into a network CIDR: a bare host address expands to
// its /24 network, and an explicit prefix gets its host bits cleared. The
// prefix is matched as digits only — an empty prefix ("192.168.1.4/") is
// rejected instead of falling through Number('') === 0 into "0.0.0.0/0".
export function toNetworkCidr(input: string): string | null {
    const trimmed = input.trim();
    if (!trimmed) return null;
    const [addr, ...rest] = trimmed.split('/');
    if (rest.length > 1) return null;
    const ip = parseIpv4(addr);
    if (!ip) return null;
    if (rest.length === 0) {
        return `${ip[0]}.${ip[1]}.${ip[2]}.0/24`;
    }
    if (!CIDR_PREFIX.test(rest[0])) return null;
    const prefix = Number(rest[0]);
    if (prefix > 32) return null;
    const mask = prefix === 0 ? 0 : (0xffffffff << (32 - prefix)) >>> 0;
    const ipNum = ((ip[0] << 24) | (ip[1] << 16) | (ip[2] << 8) | ip[3]) >>> 0;
    const network = (ipNum & mask) >>> 0;
    return [24, 16, 8, 0].map(shift => (network >> shift) & 0xff).join('.') + '/' + prefix;
}
