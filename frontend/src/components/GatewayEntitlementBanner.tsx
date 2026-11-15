// GatewayEntitlementBanner.tsx — Gateway ログイン (feature/gateway-login)
//
// 設計 SSOT: 内部設計メモ §A-2-4
// ゲートウェイ経由の転写で 403 (プラン無効) / 429 (上限到達) を受けたときの
// インラインバナー。L337 (alert/confirm/prompt 禁止) 準拠。
//
// 動作:
//   - listen 'gateway:entitlement-error' → 表示 (Rust recorder.rs が emit)
//   - × ボタン or 'gateway:auth-changed' (再ログイン等) → 非表示
//   - 自動では消さない (混雑バナーと違い、ユーザーが対処すべき恒常エラーのため)
//   - 2026-07-28: 購入導線を接続。403 (プラン無効) のときだけプラン購入ボタンを出し、
//     Stripe Checkout を既定ブラウザで開く。429 は待てば直るので購入ボタンは出さない。

import { useEffect, useState } from 'react'
import { listen, type UnlistenFn } from '@tauri-apps/api/event'
import { AlertTriangle, ExternalLink, Loader2, X } from 'lucide-react'
import {
  gatewayOpenCheckout,
  PLAN_CATALOG,
  type GatewayEntitlementError,
  type PurchasablePlan,
} from '../lib/tauri'

export function GatewayEntitlementBanner() {
  const [error, setError] = useState<GatewayEntitlementError | null>(null)
  const [busyPlan, setBusyPlan] = useState<PurchasablePlan | null>(null)
  const [checkoutError, setCheckoutError] = useState<string | null>(null)

  useEffect(() => {
    let mounted = true
    const unlistens: UnlistenFn[] = []

    ;(async () => {
      try {
        unlistens.push(
          await listen<GatewayEntitlementError>('gateway:entitlement-error', (e) => {
            if (!mounted) return
            setError(e.payload)
            setCheckoutError(null)
          }),
          // 再ログイン / ログアウトで状態が変わったらバナーを消す
          await listen('gateway:auth-changed', () => {
            if (!mounted) return
            setError(null)
            setCheckoutError(null)
          }),
        )
      } catch (err) {
        console.warn('GatewayEntitlementBanner listen failed:', err)
      }
    })()

    return () => {
      mounted = false
      for (const u of unlistens) u()
    }
  }, [])

  async function handleBuy(plan: PurchasablePlan) {
    if (busyPlan) return
    setCheckoutError(null)
    setBusyPlan(plan)
    try {
      await gatewayOpenCheckout(plan)
      // ブラウザ側で完了するため、バナーはユーザーが閉じるまで残す
      // (反映は数十秒かかるので「消えた=完了」と誤解させない)
    } catch (e) {
      setCheckoutError(String(e))
    } finally {
      setBusyPlan(null)
    }
  }

  if (!error) return null

  // 403 = 契約が無効 → 購入で解決できる。429 = 上限 → 待てば解決するので出さない。
  // 403 でもログイン切れ (invalid_token) は再ログインで直るので出さない (2026-11-11 利用者 OK)。
  const showPurchase = error.status === 403 && error.reason !== 'login_expired'

  return (
    <div
      role="alert"
      aria-live="assertive"
      className="flex flex-col gap-2 px-4 py-2 rounded-md bg-red-500/15 border border-red-500/50 text-red-200 shadow-lg text-sm animate-in fade-in slide-in-from-top-2"
    >
      <div className="flex items-center gap-2">
        <AlertTriangle size={14} className="shrink-0 text-red-400" />
        <span className="flex-1">
          {error.message}
          <span className="ml-2 text-xs text-red-300/70">(HTTP {error.status})</span>
        </span>
        <button
          type="button"
          onClick={() => setError(null)}
          aria-label="バナーを閉じる"
          className="p-1 rounded hover:bg-red-500/20 text-red-300"
        >
          <X size={12} />
        </button>
      </div>

      {showPurchase && (
        <div className="flex flex-wrap items-center gap-2 pl-6">
          {PLAN_CATALOG.map(({ plan, label, price }) => (
            <button
              key={plan}
              type="button"
              onClick={() => void handleBuy(plan)}
              disabled={busyPlan !== null}
              className="inline-flex items-center gap-1.5 px-2.5 py-1 rounded bg-red-500/20 hover:bg-red-500/30 border border-red-500/50 text-xs text-red-100 disabled:opacity-50"
            >
              {busyPlan === plan ? (
                <Loader2 size={11} className="animate-spin" />
              ) : (
                <ExternalLink size={11} />
              )}
              <span>
                {label} {price}
              </span>
            </button>
          ))}
          <span className="text-[11px] text-red-300/70">ブラウザで購入手続きが開きます</span>
        </div>
      )}

      {checkoutError && (
        <div className="pl-6 text-[11px] text-amber-300/90">{checkoutError}</div>
      )}
    </div>
  )
}
