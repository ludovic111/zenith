import { useEffect, useRef } from "react";

import { peekPairingTokenFromUrl, stripPairingTokenFromUrl } from "../environments/primary/auth";
import { isEmbedded, requestEmbeddedPairingToken } from "./embed";

/**
 * On the pairing screen, sign in without user action: inside a parent page (an
 * iframe), fetch a one-time token from it; otherwise take a token put in the URL
 * (`#token=`, as in zenith's "Open in Browser" link to `/pair#token=…`) after this
 * screen showed. A token already in the URL on load is handled by the screen.
 */
export function useZenithEmbeddedPairing(input: {
  readonly skip: boolean;
  readonly submit: (token: string) => Promise<void>;
  readonly setPending: (pending: boolean) => void;
  readonly setError: (message: string) => void;
}) {
  const attemptedRef = useRef(false);
  const { skip, submit, setPending, setError } = input;

  useEffect(() => {
    if (skip || attemptedRef.current || !isEmbedded()) return;
    attemptedRef.current = true;
    setPending(true);
    void requestEmbeddedPairingToken().then((result) => {
      if (result.kind === "token") {
        void submit(result.token);
        return;
      }
      setPending(false);
      if (result.kind === "error") setError(result.message);
    });
  }, [skip, submit, setPending, setError]);

  useEffect(() => {
    if (skip) return;
    const onHashChange = () => {
      const token = peekPairingTokenFromUrl();
      if (!token || attemptedRef.current) return;
      attemptedRef.current = true;
      stripPairingTokenFromUrl();
      void submit(token);
    };
    window.addEventListener("hashchange", onHashChange);
    return () => window.removeEventListener("hashchange", onHashChange);
  }, [skip, submit]);
}
