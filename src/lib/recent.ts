const RECENT_WINDOW_MS = 7 * 24 * 60 * 60 * 1_000;

export function isRecentItem(createdAt: string, now = Date.now()) {
  const createdAtMs = Date.parse(createdAt);
  return Number.isFinite(createdAtMs) && createdAtMs >= now - RECENT_WINDOW_MS;
}
