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
})
