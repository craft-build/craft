(function () {
  function addAnchors() {
    document.querySelectorAll('.content main h1[id], .content main h2[id], .content main h3[id]').forEach(function (h) {
      if (h.querySelector('.header-anchor')) return;
      var a = document.createElement('a');
      a.className = 'header-anchor';
      a.href = '#' + h.id;
      a.textContent = '#';
      h.appendChild(a);
    });
  }
  if (document.readyState !== 'loading') addAnchors();
  else document.addEventListener('DOMContentLoaded', addAnchors);
})();
