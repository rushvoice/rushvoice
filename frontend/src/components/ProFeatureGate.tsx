// ProFeatureGate.tsx — プラン別の機能ゲート (2026-07-28 決定)
//
// 上位プラン (プロ) 限定機能 = 議事録モード / セミナー字幕 / 優先サポート。
// 判定規則は Rust 側 gateway_auth::plan_allows_pro_features が正 (pro / trial / 未ログイン=許可)。
//
// 設計上の割り切り:
//   - これは **UX 上のゲートであって安全境界ではない**。課金の正当性はゲートウェイ側の
//     entitlement 検証が担保する。
//   - 判定は config_db の写しで即時に返し、裏で gateway_status() を叩いて写しを更新する
//     (購入直後に「まだプロになっていない」と表示され続けるのを防ぐ)。
//   - 判定中はゲートを掛けない (許可側に倒す)。一瞬のロック画面のちらつきを避けるため。

import { useCallback, useEffect, useState } from 'react'
import { listen, type UnlistenFn } from '@tauri-apps/api/event'
import { ExternalLink, Loader2, Lock, RefreshCw } from 'lucide-react'
import {
  gatewayOpenCheckout,
  gatewayProFeatureAccess,
  gatewayStatus,
  type ProFeatureAccess,
  type PurchasablePlan,
} from '../lib/tauri'

const UPGRADE_PLAN: PurchasablePlan = 'pro'

export function ProFeatureGate({
  featureName,
  description,
  children,
}: {
  /** 「議事録モード」等・ロック画面の見出しに使う */
  featureName: string
  /** その機能で何ができるかの 1 行説明 */
  description: string
  children: React.ReactNode
}) {
  const [access, setAccess] = useState<ProFeatureAccess | null>(null)
  const [busy, setBusy] = useState(false)
  const [error, setError] = useState<string | null>(null)

  const refresh = useCallback(async (viaNetwork: boolean) => {
    try {
      if (viaNetwork) {
        // /v1/me を叩いて Rust 側のプラン写しを更新させる (戻り値は使わない)
        await gatewayStatus().catch(() => null)
      }
      const a = await gatewayProFeatureAccess()
      setAccess(a)
    } catch (e) {
      console.warn('[pro-gate] access check failed:', e)
      // 判定できないときは許可側に倒す (課金の正当性は gateway が担保)
      setAccess({ allowed: true, plan: null, logged_in: false })
    }
  }, [])

  useEffect(() => {
    let mounted = true
    const unlistens: UnlistenFn[] = []
    ;(async () => {
      await refresh(false) // まず写しで即時判定
      if (!mounted) return
      void refresh(true) // 裏で最新化
      try {
        unlistens.push(
          await listen('gateway:auth-changed', () => {
            if (mounted) void refresh(false)
          }),
        )
      } catch (e) {
        console.warn('[pro-gate] auth-changed listen failed:', e)
      }
    })()
    return () => {
      mounted = false
      for (const u of unlistens) u()
    }
  }, [refresh])

  async function handleUpgrade() {
    if (busy) return
    setError(null)
    setBusy(true)
    try {
      await gatewayOpenCheckout(UPGRADE_PLAN)
    } catch (e) {
      setError(String(e))
    } finally {
      setBusy(false)
    }
  }

  // 判定前 / 許可はそのまま通す
  if (access === null || access.allowed) return <>{children}</>

  return (
    <div className="flex-1 flex items-center justify-center p-8">
      <div className="max-w-md w-full space-y-4 p-6 bg-neutral-950 border border-neutral-800 rounded-lg text-center">
        <div className="flex justify-center">
          <div className="p-3 rounded-full bg-neutral-800">
            <Lock size={20} className="text-neutral-400" />
          </div>
        </div>
        <div>
          <h2 className="text-base font-medium text-neutral-100">
            {featureName}はプロプラン限定です
          </h2>
          <p className="text-xs text-neutral-500 mt-1.5 leading-relaxed">{description}</p>
        </div>
        <div className="text-xs text-neutral-400">
          現在のプラン: {access.plan ?? '不明'}
        </div>
        <div className="flex flex-col gap-2">
          <button
            type="button"
            onClick={() => void handleUpgrade()}
            disabled={busy}
            className="inline-flex items-center justify-center gap-1.5 px-4 py-2.5 bg-emerald-700 hover:bg-emerald-600 text-white text-sm rounded-md disabled:opacity-50"
          >
            {busy ? <Loader2 size={14} className="animate-spin" /> : <ExternalLink size={14} />}
            <span>プロにアップグレード（¥1,980/月）</span>
          </button>
          <button
            type="button"
            onClick={() => void refresh(true)}
            className="inline-flex items-center justify-center gap-1.5 px-3 py-2 text-xs text-neutral-400 hover:text-neutral-200"
          >
            <RefreshCw size={12} />
            <span>購入済みの場合はここで再確認</span>
          </button>
        </div>
        {error && (
          <p role="alert" className="text-xs text-red-300/90">
            {error}
          </p>
        )}
      </div>
    </div>
  )
}
