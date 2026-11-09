import { describe, expect, it } from "vitest";
import { fmtDur, fmtGiB, fmtMs, fmtNum, fmtUptime } from "./format";

describe("fmtNum", () => {
  it("renders small numbers verbatim", () => {
    expect(fmtNum(0)).toBe("0");
    expect(fmtNum(42)).toBe("42");
    expect(fmtNum(999)).toBe("999");
  });
  it("compacts thousands/millions/billions", () => {
    expect(fmtNum(1000)).toBe("1.0k");
    expect(fmtNum(5000)).toBe("5.0k");
    expect(fmtNum(1234)).toBe("1.2k");
    expect(fmtNum(1_500_000)).toBe("1.5M");
    expect(fmtNum(2_500_000_000)).toBe("2.5B");
  });
  it("handles null/NaN", () => {
    expect(fmtNum(null)).toBe("—");
    expect(fmtNum(undefined)).toBe("—");
    expect(fmtNum(NaN)).toBe("—");
  });
});

describe("fmtMs / fmtDur", () => {
  it("keeps sub-second in ms", () => {
    expect(fmtMs(0)).toBe("0ms");
    expect(fmtMs(250)).toBe("250ms");
    expect(fmtMs(850)).toBe("850ms");
  });
  it("rolls over to seconds", () => {
    expect(fmtMs(1000)).toBe("1s");
    expect(fmtMs(999.5)).toBe("1.0s");
    expect(fmtMs(1200)).toBe("1.2s");
    expect(fmtMs(3200)).toBe("3.2s");
    expect(fmtMs(45_000)).toBe("45s");
  });
  it("scales minutes/hours/days", () => {
    expect(fmtMs(60_000)).toBe("1m");
    expect(fmtMs(150_000)).toBe("2m 30s");
    expect(fmtMs(4_200_000)).toBe("1h 10m");
    expect(fmtMs(200_000_000)).toBe("2d 7h");
  });
  it("fmtDur without sub collapses sub-second", () => {
    expect(fmtDur(0.4)).toBe("<1s");
    expect(fmtDur(45)).toBe("45s");
  });
  it("handles null/NaN", () => {
    expect(fmtMs(null)).toBe("—");
    expect(fmtDur(undefined)).toBe("—");
  });
});

describe("fmtUptime", () => {
  it("scales up", () => {
    expect(fmtUptime(30)).toBe("0m 30s");
    expect(fmtUptime(4200)).toBe("1h 10m");
    expect(fmtUptime(200_000)).toBe("2d 7h");
    expect(fmtUptime(null)).toBe("—");
  });
});

describe("fmtGiB", () => {
  it("converts bytes", () => {
    expect(fmtGiB(0)).toBe("0.0");
    expect(fmtGiB(16 * 1024 ** 3)).toBe("16.0");
    expect(fmtGiB(1.5 * 1024 ** 3)).toBe("1.5");
    expect(fmtGiB(null)).toBe("—");
  });
});
