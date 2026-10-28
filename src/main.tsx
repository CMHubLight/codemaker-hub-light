import { StrictMode } from 'react'
import { createRoot } from 'react-dom/client'
import App from './App'
import { ErrorBoundary } from './components/ErrorBoundary'
import './styles.css'

const container = document.getElementById('root')
if (!container) {
  throw new Error('未找到 #root 挂载节点，请检查 index.html')
}

createRoot(container).render(
  <StrictMode>
    <ErrorBoundary scope="应用" reloadOnReset>
      <App />
    </ErrorBoundary>
  </StrictMode>
)
