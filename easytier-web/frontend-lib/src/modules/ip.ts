const IPV4_ADDRESS = /^(25[0-5]|2[0-4]\d|1\d\d|[1-9]?\d)(\.(25[0-5]|2[0-4]\d|1\d\d|[1-9]?\d)){3}$/;

// The comprehensive IPv6 form, including an embedded IPv4 tail such as
// ::ffff:10.0.0.2. Leading zeros inside a group are not accepted, matching
// the Rust-side IpAddr parser.
const IPV6_ADDRESS = new RegExp('^(' +
  '([0-9a-fA-F]{1,4}:){7}[0-9a-fA-F]{1,4}' +
  '|([0-9a-fA-F]{1,4}:){1,7}:' +
  '|([0-9a-fA-F]{1,4}:){1,6}:[0-9a-fA-F]{1,4}' +
  '|([0-9a-fA-F]{1,4}:){1,5}(:[0-9a-fA-F]{1,4}){1,2}' +
  '|([0-9a-fA-F]{1,4}:){1,4}(:[0-9a-fA-F]{1,4}){1,3}' +
  '|([0-9a-fA-F]{1,4}:){1,3}(:[0-9a-fA-F]{1,4}){1,4}' +
  '|([0-9a-fA-F]{1,4}:){1,2}(:[0-9a-fA-F]{1,4}){1,5}' +
  '|[0-9a-fA-F]{1,4}:((:[0-9a-fA-F]{1,4}){1,6})' +
  '|:((:[0-9a-fA-F]{1,4}){1,7}|:)' +
  '|::(ffff(:0{1,4})?:)?((25[0-5]|2[0-4]\\d|1\\d\\d|[1-9]?\\d)\\.){3}(25[0-5]|2[0-4]\\d|1\\d\\d|[1-9]?\\d)' +
  '|([0-9a-fA-F]{1,4}:){1,4}:((25[0-5]|2[0-4]\\d|1\\d\\d|[1-9]?\\d)\\.){3}(25[0-5]|2[0-4]\\d|1\\d\\d|[1-9]?\\d)' +
  ')$');

/** A bare IPv4 or IPv6 address, without a port or brackets. */
export function isValidHostIp(value: string | null | undefined): boolean {
  const text = (value ?? '').trim();
  if (!text) return false;
  return text.includes(':') ? IPV6_ADDRESS.test(text) : IPV4_ADDRESS.test(text);
}
