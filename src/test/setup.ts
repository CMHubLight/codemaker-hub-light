// 全局测试脚手架：仅为 DOM 环境注册 jest-dom 断言（toBeInTheDocument 等）。
// node 环境（纯逻辑测试）跳过动态导入，避免为每个测试文件加载整套 DOM 断言扩展。
if (typeof document !== 'undefined') {
  await import('@testing-library/jest-dom/vitest')
  // 本项目没有开启 vitest globals，React Testing Library 不会自动注册 afterEach 清理；
  // 不清理会让上一个用例的 DOM 留在 document 里，导致后续 getByText 命中重复节点。
  const [{ cleanup }, { afterEach }] = await Promise.all([
    import('@testing-library/react'),
    import('vitest')
  ])
  afterEach(cleanup)
}
