import { useEffect, useRef } from "react";

import { isEmbedded, requestEmbeddedPairingToken } from "./embed";

/**
 * On the pairing screen inside zenith, fetch a one-time token from the parent
 * page and submit it, so the embedded app signs in without user action.
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
}
