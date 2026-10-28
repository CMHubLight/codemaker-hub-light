import { useState } from 'react'

interface CodeBlockProps {
  title: string
  note: string
  code: string
}

/** 接入说明代码块：只读展示 + 剪贴板复制（复制失败降级为手动选择）。 */
export function CodeBlock({ title, note, code }: CodeBlockProps) {
  const [copyState, setCopyState] = useState<'idle' | 'copied' | 'failed'>('idle')

  async function handleCopy() {
    try {
      await navigator.clipboard.writeText(code)
      setCopyState('copied')
      window.setTimeout(() => setCopyState('idle'), 1500)
    } catch {
      setCopyState('failed')
      window.setTimeout(() => setCopyState('idle'), 2000)
    }
  }

  return (
    <article className="code-block">
      <header className="code-block-header">
        <div>
          <h3>{title}</h3>
          <p>{note}</p>
        </div>
        <button type="button" className="btn btn--ghost btn--sm" onClick={() => void handleCopy()}>
          {copyState === 'copied' ? '已复制' : copyState === 'failed' ? '复制失败' : '复制'}
        </button>
      </header>
      <pre>
        <code>{code}</code>
      </pre>
    </article>
  )
}
