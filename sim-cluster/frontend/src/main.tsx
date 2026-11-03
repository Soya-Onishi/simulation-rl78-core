import { createRoot } from 'react-dom/client'
import './index.css'
import App from './App.tsx'

// StrictMode is intentionally not used: it double-mounts effects in dev, which
// would open (and drop) the single-viewer SSE connection twice.
createRoot(document.getElementById('root')!).render(<App />)
