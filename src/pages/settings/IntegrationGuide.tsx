import { Panel, PanelHeader } from '../../components/Panel'
import { CodeBlock } from './CodeBlock'

/** 接入说明面板：三种客户端的最小配置示例（Base URL 指向本地代理）。 */
export function IntegrationGuide({ baseUrl }: { baseUrl: string }) {
  return (
    <Panel>
      <PanelHeader
        title="接入说明"
        meta="把 CLI / SDK 的 Base URL 指向本地代理，鉴权头由代理统一注入"
      />

      <div className="code-blocks">
        <CodeBlock
          title="Claude Code"
          note="环境变量（PowerShell / bash 通用）"
          code={`# 鉴权由本地代理注入，客户端 key 仅作占位
ANTHROPIC_BASE_URL=${baseUrl}
ANTHROPIC_API_KEY=codemaker-hub-local`}
        />
        <CodeBlock
          title="Codex"
          note="~/.codex/config.toml（OPENAI_API_KEY 填占位值）"
          code={`# ~/.codex/config.toml
model_provider = "codemaker"

[model_providers.codemaker]
name = "CodeMaker Hub Light"
base_url = "${baseUrl}/v1"
env_key = "OPENAI_API_KEY"
wire_api = "responses"`}
        />
        <CodeBlock
          title="OpenAI SDK"
          note="Python"
          code={`from openai import OpenAI

client = OpenAI(
    base_url="${baseUrl}/v1",
    api_key="codemaker-hub-local",  # 占位，代理会剥离并注入真实凭据
)`}
        />
      </div>

      <p className="hint">
        <code>/v1/messages</code> 会在本地转换为 OpenAI Chat（网关拒绝原生 Anthropic），
        <code>/v1/chat/completions</code> 与 <code>/v1/responses</code> 同协议转发。
        前端、日志与数据库都不会出现 token。
      </p>
    </Panel>
  )
}
