import { useCallback, useEffect, useState } from "react";
import DiskList from "./components/DiskList";
import OperationModal, { type Operation } from "./components/OperationModal";
import TransactionLogPanel from "./components/TransactionLogPanel";
import { listDisks } from "./lib/disks";
import type { Disk } from "./types";

type LoadState =
  | { kind: "loading" }
  | { kind: "ready"; disks: Disk[] }
  | { kind: "error"; message: string };

export default function App() {
  const [state, setState] = useState<LoadState>({ kind: "loading" });
  const [activeOp, setActiveOp] = useState<Operation | null>(null);
  // 트랜잭션 로그 패널 refresh 트리거 — 작업 성공 시 increment.
  const [txnRefreshKey, setTxnRefreshKey] = useState(0);

  const refresh = useCallback(() => {
    setState({ kind: "loading" });
    listDisks()
      .then((disks) => setState({ kind: "ready", disks }))
      .catch((err: unknown) => {
        const message =
          typeof err === "string"
            ? err
            : err instanceof Error
              ? err.message
              : String(err);
        setState({ kind: "error", message });
      });
  }, []);

  useEffect(() => {
    refresh();
  }, [refresh]);

  return (
    <main className="min-h-screen px-6 py-8">
      <header className="mb-8 flex items-end justify-between">
        <div>
          <h1 className="text-3xl font-semibold tracking-tight">Parq</h1>
          <p className="mt-1 text-sm text-neutral-400">
            Open source partition manager for Windows.
          </p>
        </div>
        <span className="rounded border border-neutral-700 px-2 py-0.5 text-xs text-neutral-400">
          V0
        </span>
      </header>

      <section className="mb-3 flex items-center justify-between">
        <h2 className="text-sm font-medium uppercase tracking-wide text-neutral-400">
          디스크
        </h2>
        <button
          type="button"
          onClick={refresh}
          className="text-xs text-neutral-500 hover:text-neutral-300"
        >
          새로고침
        </button>
      </section>

      {state.kind === "loading" && (
        <p className="text-sm text-neutral-500">디스크 열거 중...</p>
      )}
      {state.kind === "error" && (
        <div className="rounded border border-red-900 bg-red-950/40 p-4 text-sm text-red-300">
          <p className="font-medium">디스크를 불러오지 못했습니다.</p>
          <p className="mt-1 text-xs text-red-400">{state.message}</p>
        </div>
      )}
      {state.kind === "ready" && (
        <DiskList disks={state.disks} onOperation={setActiveOp} />
      )}

      <div className="mt-10">
        <TransactionLogPanel refreshKey={txnRefreshKey} />
      </div>

      <OperationModal
        operation={activeOp}
        onClose={() => setActiveOp(null)}
        onSuccess={() => {
          refresh();
          setTxnRefreshKey((k) => k + 1);
        }}
      />
    </main>
  );
}
