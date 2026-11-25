/** 9 位连接码按 `572 955 600` 分组显示，方便口头转述。 */
export function formatConnectCode(code: string): string {
  const digits = code.replace(/\D/g, "");
  if (digits.length !== 9) return code;
  return `${digits.slice(0, 3)} ${digits.slice(3, 6)} ${digits.slice(6)}`;
}
