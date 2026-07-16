export function formatAmountCents(cents: number) {
  const value = BigInt(cents);
  const negative = value < 0n;
  const absolute = negative ? -value : value;
  const yuan = absolute / 100n;
  const remainder = (absolute % 100n).toString().padStart(2, "0");
  return `${negative ? "-" : ""}${yuan}.${remainder}`;
}
