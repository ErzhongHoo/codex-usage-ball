import { describe, expect, it } from "vitest";
import {
  normalizeBallSize,
  normalizeAppSettings,
  normalizeProxyUrl,
  normalizeRefreshIntervalMinutes,
} from "./settings";

describe("normalizeAppSettings", () => {
  it("使用默认值归一化主题相关配置", () => {
    expect(normalizeAppSettings({ language: "zh-CN", themeMode: "light" })).toMatchObject({
      language: "zh-CN",
      themeMode: "light",
      refreshIntervalMinutes: 5,
      ballSize: 112,
      launchAtLogin: false,
      proxyEnabled: false,
      proxyUrl: "",
      activeRateLimitId: "__default__",
    });
  });

  it("将旧版固定刷新频率迁移为默认 5 分钟", () => {
    expect(normalizeAppSettings({ refreshIntervalSec: 180 } as unknown)).toMatchObject({
      refreshIntervalMinutes: 5,
    });
  });

  it("允许自定义刷新频率且最低为 1 分钟", () => {
    expect(normalizeAppSettings({ refreshIntervalMinutes: 12 })).toMatchObject({
      refreshIntervalMinutes: 12,
    });
    expect(normalizeAppSettings({ refreshIntervalMinutes: 0 })).toMatchObject({
      refreshIntervalMinutes: 1,
    });
    expect(normalizeRefreshIntervalMinutes(Number.NaN)).toBe(5);
  });

  it("允许在 88 到 160 像素之间设置悬浮球大小", () => {
    expect(normalizeAppSettings({ ballSize: 136 })).toMatchObject({ ballSize: 136 });
    expect(normalizeBallSize(40)).toBe(88);
    expect(normalizeBallSize(200)).toBe(160);
    expect(normalizeBallSize(Number.NaN)).toBe(112);
  });

  it("保留用户开启的开机启动配置", () => {
    expect(normalizeAppSettings({ launchAtLogin: true })).toMatchObject({
      launchAtLogin: true,
    });
  });

  it("保留并清理代理配置", () => {
    expect(normalizeAppSettings({
      proxyEnabled: true,
      proxyUrl: "  http://127.0.0.1:7890  ",
    })).toMatchObject({
      proxyEnabled: true,
      proxyUrl: "http://127.0.0.1:7890",
    });
  });

  it("非法代理配置回退为关闭和空地址", () => {
    expect(normalizeAppSettings({ proxyEnabled: "true", proxyUrl: 7890 })).toMatchObject({
      proxyEnabled: false,
      proxyUrl: "",
    });
    expect(normalizeProxyUrl("x".repeat(2200))).toHaveLength(2048);
  });

  it("只保留浅色和深色主题，旧系统主题迁移为浅色", () => {
    expect(normalizeAppSettings({ themeMode: "dark" })).toMatchObject({ themeMode: "dark" });
    expect(normalizeAppSettings({ themeMode: "light" })).toMatchObject({ themeMode: "light" });
    expect(normalizeAppSettings({ themeMode: "system" as unknown })).toMatchObject({
      themeMode: "light",
    });
  });

  it("低额度提醒阈值默认是 15", () => {
    expect(normalizeAppSettings({})).toMatchObject({
      lowNoticeThreshold: 15,
    });
  });

  it("允许 1-100 的阈值设置", () => {
    expect(normalizeAppSettings({ lowNoticeThreshold: 42 })).toMatchObject({
      lowNoticeThreshold: 42,
    });
  });

  it("低于 1 或高于 100 都会被限制边界", () => {
    expect(normalizeAppSettings({ lowNoticeThreshold: 0 })).toMatchObject({
      lowNoticeThreshold: 1,
    });
    expect(normalizeAppSettings({ lowNoticeThreshold: 101 })).toMatchObject({
      lowNoticeThreshold: 100,
    });
    expect(normalizeAppSettings({ lowNoticeThreshold: "15" })).toMatchObject({
      lowNoticeThreshold: 15,
    });
  });

  it("保留有效的额度桶设置值", () => {
    expect(normalizeAppSettings({ activeRateLimitId: "codex_spark" })).toMatchObject({
      activeRateLimitId: "codex_spark",
    });
  });

  it("非法额度桶设置回退为默认值", () => {
    expect(normalizeAppSettings({ activeRateLimitId: 123 as unknown })).toMatchObject({
      activeRateLimitId: "__default__",
    });
  });

  it("缺省时使用默认额度桶", () => {
    expect(normalizeAppSettings({})).toMatchObject({
      activeRateLimitId: "__default__",
    });
  });
});
