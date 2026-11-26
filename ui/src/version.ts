/**
 * 版本号比较：`actual` 低于 `min` 时返回 `true`。
 *
 * 与 Rust 侧 `update::version_gt` 同口径：忽略前导 `v`，按 `.` 分段取前导
 * 数字比较（`1.2.10 > 1.2.9`），缺省段按 0，`-beta` / `+build` 后缀不参与。
 * 解析不出来时当作「不低于」，宁可不多提示，也不误报服务器过旧。
 */
export function versionBelow(
  actual: string | null | undefined,
  min: string,
): boolean {
  if (!actual) return false;
  const parse = (raw: string): number[] =>
    raw
      .trim()
      .replace(/^v/i, "")
      .split(/[-+]/)[0]
      .split(".")
      .map((part) => Number.parseInt(part, 10) || 0);
  const left = parse(actual);
  const right = parse(min);
  for (let index = 0; index < Math.max(left.length, right.length); index += 1) {
    const a = left[index] ?? 0;
    const b = right[index] ?? 0;
    if (a !== b) return a < b;
  }
  return false;
}
