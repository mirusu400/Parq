import { invoke } from "@tauri-apps/api/core";
import type { TransactionLog } from "../types";

export async function listTransactions(): Promise<TransactionLog[]> {
  return invoke<TransactionLog[]>("list_transactions");
}
