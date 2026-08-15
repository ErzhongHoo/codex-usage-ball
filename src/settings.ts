export type Language = "zh-CN" | "en-US";
export type ThemeMode = "light" | "dark";

export type AppSettings = {
  language: Language;
  themeMode: ThemeMode;
  refreshIntervalSec: 180;
  launchAtLogin: boolean;
  lowNoticeThreshold: number;
  proxyEnabled: boolean;
  proxyUrl: string;
  activeRateLimitId: string;
};

export const defaultSettings: AppSettings = {
  language: "zh-CN",
  themeMode: "light",
  refreshIntervalSec: 180,
  launchAtLogin: false,
  lowNoticeThreshold: 15,
  proxyEnabled: false,
  proxyUrl: "",
  activeRateLimitId: "__default__",
};

export function normalizeLowNoticeThreshold(value: unknown): number {
  if (typeof value !== "number" || !Number.isFinite(value)) {
    return defaultSettings.lowNoticeThreshold;
  }

  return Math.min(100, Math.max(1, Math.round(value)));
}

export function normalizeProxyUrl(value: unknown): string {
  return typeof value === "string" ? value.trim().slice(0, 2048) : "";
}

export function normalizeAppSettings(value: unknown): AppSettings {
  const parsed =
    value && typeof value === "object" ? (value as Partial<AppSettings>) : {};

  return {
    language: parsed.language === "en-US" ? "en-US" : "zh-CN",
    themeMode: parsed.themeMode === "dark" ? "dark" : "light",
    // Migrate the previous 30/60-second choices to the fixed 3-minute interval.
    refreshIntervalSec: 180,
    launchAtLogin: parsed.launchAtLogin === true,
    lowNoticeThreshold: normalizeLowNoticeThreshold(parsed.lowNoticeThreshold),
    proxyEnabled: parsed.proxyEnabled === true,
    proxyUrl: normalizeProxyUrl(parsed.proxyUrl),
    activeRateLimitId:
      typeof parsed.activeRateLimitId === "string" ? parsed.activeRateLimitId : defaultSettings.activeRateLimitId,
  };
}
