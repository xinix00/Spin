// Keep this entry point usable by cached dashboards that load a classic script.
// Relative imports retain the dashboard's immutable asset version.
import(new URL('./ui/app.js', document.currentScript.src).href).catch(error => {
  console.error('Spin kon niet worden geladen', error);
  const message = document.getElementById('auth-copy');
  if (message) message.textContent = 'Spin kon niet worden geladen. Vernieuw de pagina om opnieuw te proberen.';
});
