import { StrictMode } from 'react'
import { createRoot } from 'react-dom/client'
import './index.css'
import App from './App.tsx'
import { Overlay } from './pages/Overlay.tsx'
import { SubtitleWindow } from './pages/SubtitleWindow.tsx'

// Stage 2-C-2: hash-based routing で overlay window を分岐
// Tauri 側 WebviewWindowBuilder が "index.html#/overlay" で開く
const isOverlay = window.location.hash === '#/overlay'
// セミナー字幕ウィンドウ (feature/seminar-subtitle): "index.html#/subtitle" で開く
const isSubtitle = window.location.hash === '#/subtitle'

// overlay モードでは index.css の html/body/#root 背景 (#171717) が Tauri の
// transparent: true を潰すため、透明に上書きする (実機検証で「波形位置が黒塗り」事案)
if (isOverlay) {
  document.documentElement.style.background = 'transparent'
  document.body.style.background = 'transparent'
  const rootEl = document.getElementById('root')
  if (rootEl) rootEl.style.background = 'transparent'
}

createRoot(document.getElementById('root')!).render(
  <StrictMode>
    {isOverlay ? <Overlay /> : isSubtitle ? <SubtitleWindow /> : <App />}
  </StrictMode>,
)
