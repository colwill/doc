// Served and loaded only when [dev] reload is on. Polls the frontend's boot id: a failing request
// means a rebuild is in flight, and a changed id means the new process is serving, so reload.
// A hidden tab does not ask at all, and a rebuild is waited out at longer and longer intervals,
// so a build that takes a while leaves no line of stalled requests behind it.
(function () {
  var interval = 1000;
  var longest = 10000;
  var wait = interval;
  var current = null;
  var timer = null;

  function later() {
    window.clearTimeout(timer);
    timer = window.setTimeout(poll, wait);
  }

  function poll() {
    if (document.hidden) return;
    fetch("/dev/boot", { cache: "no-store" })
      .then(function (response) {
        return response.ok ? response.text() : Promise.reject(response.status);
      })
      .then(function (boot) {
        wait = interval;
        if (current === null) {
          current = boot;
        } else if (boot !== current) {
          window.location.reload();
          return;
        }
        later();
      })
      .catch(function () {
        wait = Math.min(wait * 2, longest);
        later();
      });
  }

  document.addEventListener("visibilitychange", function () {
    if (!document.hidden) {
      wait = interval;
      later();
    }
  });

  poll();
})();
