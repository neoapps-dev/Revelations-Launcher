import { useState, useEffect } from "react";
import { useLocalStorage } from "./useLocalStorage";

export function useSkinSync() {
  const [skinUrl, setSkinUrl] = useLocalStorage("lce-skin", "/images/Default.png");
  const [skinBase64, setSkinBase64] = useState<string | null>(null);
  const [skinModel, setSkinModel] = useLocalStorage<string>("lce-skin-model", "steve");

  useEffect(() => {
    const syncSkin = async () => {
      if (!skinUrl) return;
      try {
        const img = new Image();
        img.crossOrigin = "anonymous";
        img.onload = () => {
          // Export as 64x32 for PCK (crop top half of 64x64 skins)
          const cvs = document.createElement("canvas");
          cvs.width = 64;
          cvs.height = 32;
          const ctx = cvs.getContext("2d");
          if (ctx) {
            ctx.imageSmoothingEnabled = false;
            const srcH = img.naturalHeight > 32 ? img.naturalHeight / 2 : img.naturalHeight;
            ctx.drawImage(img, 0, 0, img.naturalWidth, srcH, 0, 0, 64, 32);
            setSkinBase64(cvs.toDataURL("image/png"));
          }
        };
        img.src = skinUrl;
      } catch (e) {
        console.error("Skin conversion failed:", e);
      }
    };
    syncSkin();
  }, [skinUrl]);

  return {
    skinUrl,
    setSkinUrl,
    skinBase64,
    skinModel,
    setSkinModel,
  };
}
