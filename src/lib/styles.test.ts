/// <reference types="node" />
import { readFileSync } from 'node:fs'
import { describe, expect, it } from 'vitest'

// 直接读样式源文件断言：CSS 没有运行时可观测点，只能把“不该再出现的东西”钉成测试。
// tsconfig 默认只带 vite/client 类型，这里显式引入 node 类型以使用 fs（测试文件局部生效）。
const css = readFileSync(new URL('../styles.css', import.meta.url), 'utf8')

describe('styles.css', () => {
  it('styles_have_no_undefined_css_variables', () => {
    // 只认行首的声明（`  --name: value`），避免把 `.btn--primary:hover` 这类类名当成变量定义。
    const defined = new Set(
      Array.from(css.matchAll(/^[ \t]*(--[a-z0-9-]+)\s*:/gm), (match) => match[1])
    )
    const used = new Set(
      Array.from(css.matchAll(/var\(\s*(--[a-z0-9-]+)/g), (match) => match[1])
    )
    const undefinedVariables = Array.from(used).filter((name) => !defined.has(name))

    // 未定义变量会静默落回 fallback/继承值：浅色模式登录粘贴框曾因此变成深色。
    expect(undefinedVariables).toEqual([])
  })

  it('styles_have_no_unreachable_breakpoints', () => {
    const breakpoints = Array.from(
      css.matchAll(/@media\s*\(\s*max-width\s*:\s*(\d+)px\s*\)/g),
      (match) => Number(match[1])
    )

    // 窗口 minWidth=960（src-tauri/tauri.conf.json）：900/560 断点永远不会命中。
    expect(breakpoints).not.toContain(900)
    expect(breakpoints).not.toContain(560)
    // 1180 是窄窗口下统计卡变两列的有效断点，必须保留。
    expect(breakpoints).toContain(1180)
  })

  it('light_theme_status_text_keeps_aa_contrast', () => {
    expect(contrast(token('--text-muted'), token('--bg'))).toBeGreaterThanOrEqual(4.5)
    expect(contrast(token('--text-muted'), token('--bg-subtle'))).toBeGreaterThanOrEqual(4.5)
    expect(contrast(token('--success'), token('--success-soft'))).toBeGreaterThanOrEqual(4.5)
    expect(contrast(token('--accent-hover'), token('--accent-soft'))).toBeGreaterThanOrEqual(4.5)
  })
})

const lightRootCss = css.slice(0, css.indexOf('@media (prefers-color-scheme: dark)'))

function token(name: string): string {
  const escaped = name.replace(/[.*+?^${}()|[\]\\]/g, '\\$&')
  const match = lightRootCss.match(new RegExp(`${escaped}\\s*:\\s*(#[0-9a-f]{6})`, 'i'))
  if (!match) throw new Error(`缺少浅色主题 token：${name}`)
  return match[1]
}

function contrast(foreground: string, background: string): number {
  const fg = luminance(foreground)
  const bg = luminance(background)
  const [light, dark] = fg > bg ? [fg, bg] : [bg, fg]
  return (light + 0.05) / (dark + 0.05)
}

function luminance(hex: string): number {
  const [, r, g, b] = /^#([0-9a-f]{2})([0-9a-f]{2})([0-9a-f]{2})$/i.exec(hex) ?? []
  if (!r || !g || !b) throw new Error(`非法颜色：${hex}`)
  const [red, green, blue] = [r, g, b].map((part) => channel(Number.parseInt(part, 16)))
  return 0.2126 * red + 0.7152 * green + 0.0722 * blue
}

function channel(value: number): number {
  const normalized = value / 255
  return normalized <= 0.03928
    ? normalized / 12.92
    : ((normalized + 0.055) / 1.055) ** 2.4
}
