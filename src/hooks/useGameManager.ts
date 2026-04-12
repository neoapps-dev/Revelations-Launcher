import { useState, useEffect, useCallback, useMemo } from "react";
import { getCurrentWindow } from "@tauri-apps/api/window";
import { TauriService } from "../services/TauriService";

const appWindow = getCurrentWindow();

const BASE_EDITIONS = [
  {
    id: "revelations_edition",
    name: "LCE Revelations",
    desc: "Legacy Console Edition Revelations. Bundled with a fully-implemented hardcore mode, performance/stability optimizations, and uncapped FPS via a VSync toggle. Security-hardened with token-based encryption.",
    url: "https://github.com/itsRevela/LCE-Revelations/releases/download/Nightly/LCE-Revelations-Client-Win64.zip",
    titleImage: "/images/minecraft_title_revelations.png",
    credits: {
      developer: "itsRevela",
      platform: "github",
      url: "https://github.com/itsRevela"
    }
  },
];

const PARTNERSHIP_SERVERS: { name: string; ip: string; port: number }[] = [];

interface GameManagerProps {
  profile: string;
  setProfile: (id: string) => void;
  customEditions: any[];
  setCustomEditions: (editions: any[]) => void;
  keepLauncherOpen: boolean;
}

export function useGameManager({ profile, setProfile, customEditions, setCustomEditions, keepLauncherOpen }: GameManagerProps) {
  const [installs, setInstalls] = useState<string[]>([]);
  const [isGameRunning, setIsGameRunning] = useState(false);
  const [downloadProgress, setDownloadProgress] = useState<number | null>(null);
  const [downloadingId, setDownloadingId] = useState<string | null>(null);
  const [isRunnerDownloading, setIsRunnerDownloading] = useState(false);
  const [runnerDownloadProgress, setRunnerDownloadProgress] = useState<number | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [updateAvailable, setUpdateAvailable] = useState<Record<string, boolean>>({});

  const editions = useMemo(() => [...BASE_EDITIONS, ...customEditions], [customEditions]);

  const checkInstalls = useCallback(async () => {
    const results = await Promise.all(
      editions.map(async (e) => {
        const isInstalled = await TauriService.checkGameInstalled(e.id);
        return isInstalled ? e.id : null;
      }),
    );
    const installed = results.filter((id): id is string => id !== null);
    setInstalls(installed);

    // Check for game updates on installed base editions
    for (const id of installed) {
      if (!id.startsWith("custom_")) {
        try {
          console.log(`[UpdateCheck] Checking ${id}...`);
          const hasUpdate = await TauriService.checkForGameUpdate(id);
          console.log(`[UpdateCheck] ${id} hasUpdate=${hasUpdate}`);
          setUpdateAvailable(prev => ({ ...prev, [id]: hasUpdate }));
        } catch (err) {
          console.error(`[UpdateCheck] ${id} failed:`, err);
          setUpdateAvailable(prev => ({ ...prev, [id]: false }));
        }
      }
    }
  }, [editions]);

  useEffect(() => {
    checkInstalls();
    const unlistenDownload = TauriService.onDownloadProgress((p) => setDownloadProgress(p));
    const unlistenRunner = TauriService.onRunnerDownloadProgress((p) => setRunnerDownloadProgress(p));
    return () => {
      unlistenDownload.then((u) => u());
      unlistenRunner.then((u) => u());
    };
  }, [customEditions, checkInstalls]);

  const downloadRunner = useCallback(async (name: string, url: string) => {
    if (isRunnerDownloading) return;
    setIsRunnerDownloading(true);
    setRunnerDownloadProgress(0);
    setError(null);
    try {
      await TauriService.downloadRunner(name, url);
      setRunnerDownloadProgress(null);
    } catch (e: any) {
      console.error(e);
      setError(typeof e === 'string' ? e : e.message || "Failed to download runner");
    } finally {
      setIsRunnerDownloading(false);
    }
  }, [isRunnerDownloading]);

  const toggleInstall = useCallback(async (id: string) => {
    if (downloadingId) return;
    const edition = editions.find((e) => e.id === id);
    if (!edition) return;
    setError(null);
    try {
      setDownloadingId(id);
      setDownloadProgress(0);
      await TauriService.downloadAndInstall(edition.url, id);
      await TauriService.syncDlc(id);
      await checkInstalls();
      setProfile(id);
      setDownloadProgress(null);
      setDownloadingId(null);
    } catch (e: any) {
      console.error(e);
      setError(typeof e === 'string' ? e : e.message || "Failed to install version");
      setDownloadProgress(null);
      setDownloadingId(null);
    }
  }, [downloadingId, editions, checkInstalls, setProfile]);

  const handleUninstall = useCallback(async (id: string) => {
    await TauriService.deleteInstance(id);
    await checkInstalls();
  }, [checkInstalls]);

  const handleLaunch = useCallback(async () => {
    if (isGameRunning) return;
    setError(null);
    setIsGameRunning(true);
    try {
      if (!keepLauncherOpen) {
        await appWindow.hide();
      }
      await TauriService.launchGame(profile, PARTNERSHIP_SERVERS);
    } catch (e: any) {
      console.error(e);
      setError(typeof e === 'string' ? e : e.message || "Failed to launch game");
    } finally {
      setIsGameRunning(false);
      await appWindow.show();
      await appWindow.unminimize();
      await appWindow.setFocus();
    }
  }, [isGameRunning, profile, keepLauncherOpen]);

  const stopGame = useCallback(async () => {
    try {
      await TauriService.stopGame(profile);
      setIsGameRunning(false);
    } catch (e) {
      console.error(e);
    }
  }, [profile]);

  const addCustomEdition = useCallback((edition: { name: string; desc: string; url: string }) => {
    const id = `custom_${Date.now()}`;
    const newEdition = { ...edition, id, titleImage: "/images/minecraft_title_tucustom.png" };
    setCustomEditions([...customEditions, newEdition]);
    return id;
  }, [customEditions, setCustomEditions]);

  const deleteCustomEdition = useCallback((id: string) => {
    setCustomEditions(customEditions.filter((e) => e.id !== id));
    TauriService.deleteInstance(id).catch(console.error);
  }, [customEditions, setCustomEditions]);

  const updateCustomEdition = useCallback((id: string, updated: { name: string; desc: string; url: string }) => {
    setCustomEditions(customEditions.map((e) => e.id === id ? { ...e, ...updated } : e));
  }, [customEditions, setCustomEditions]);

  return {
    installs,
    isGameRunning,
    downloadProgress,
    downloadingId,
    isRunnerDownloading,
    runnerDownloadProgress,
    error,
    setError,
    editions,
    toggleInstall,
    handleUninstall,
    handleLaunch,
    stopGame,
    addCustomEdition,
    deleteCustomEdition,
    updateCustomEdition,
    downloadRunner,
    checkInstalls,
    updateAvailable,
  };
}
