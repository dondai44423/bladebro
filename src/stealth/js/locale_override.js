
// S6: navigator.language consistency with timezone/locale.
try{
  var _lang='__LOCALE__';
  var _langs=['__LOCALE__','__LOCALE_BASE__'];
  _defGet(Navigator.prototype,'language',_ogs(Navigator.prototype,'language'),function(th,a,og){og.apply(th,a);return _lang;});
  _defGet(Navigator.prototype,'languages',_ogs(Navigator.prototype,'languages'),function(th,a,og){og.apply(th,a);return _langs;});
}catch(e){}

